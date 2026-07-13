//! Provides an interface for the client to communicate with a Miden node using
//! Remote Procedure Calls (RPC).
//!
//! This module defines the [`NodeRpcClient`] trait which abstracts calls to the RPC protocol used
//! to:
//!
//! - Submit proven transactions.
//! - Submit proven batches.
//! - Retrieve block headers (optionally with MMR proofs).
//! - Sync state updates (including notes, nullifiers, and account updates).
//! - Fetch details for specific notes and accounts.
//!
//! The client implementation adapts to the target environment automatically:
//! - Native targets use `tonic` transport with TLS.
//! - `wasm32` targets use `tonic-web-wasm-client` transport.
//!
//! ## Example
//!
//! ```no_run
//! # use miden_client::rpc::{Endpoint, NodeRpcClient, GrpcClient};
//! # use miden_protocol::block::BlockNumber;
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create a gRPC client instance (assumes default endpoint configuration).
//! let endpoint = Endpoint::new("https".into(), "localhost".into(), Some(57291));
//! let mut rpc_client = GrpcClient::new(&endpoint, 1000);
//!
//! // Fetch the latest block header (by passing None).
//! let (block_header, mmr_proof) = rpc_client.get_block_header_by_number(None, true).await?;
//!
//! println!("Latest block number: {}", block_header.block_num());
//! if let Some(proof) = mmr_proof {
//!     println!("MMR proof received accordingly");
//! }
//!
//! #    Ok(())
//! # }
//! ```
//! The client also makes use of this component in order to communicate with the node.
//!
//! For further details and examples, see the documentation for the individual methods in the
//! [`NodeRpcClient`] trait.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use domain::account::{
    AccountDetails,
    AccountProof,
    GetAccountRequest,
    StorageMapEntries,
    StorageMapEntry,
    StorageMapFetch,
    VaultFetch,
};
use domain::note::{FetchedNote, NoteSyncBlock, SyncedNoteDetails};
use domain::nullifier::NullifierUpdate;
use domain::sync::{ChainMmrInfo, SyncTarget};
use miden_protocol::Word;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::address::NetworkId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::{BlockHeader, BlockNumber, ProvenBlock};
use miden_protocol::crypto::merkle::mmr::MmrProof;
use miden_protocol::note::{NoteId, NoteMetadata, NoteScript, NoteTag, NoteType, Nullifier};
use miden_protocol::transaction::{ProvenTransaction, TransactionInputs};

use crate::rpc::domain::storage_map::StorageMapInfo;

/// Contains domain types related to RPC requests and responses, as well as utility functions
/// for dealing with them.
pub mod domain;

mod errors;
pub use errors::*;

mod endpoint;
pub(crate) use domain::limits::RPC_LIMITS_STORE_SETTING;
pub use domain::limits::RpcLimits;
pub use domain::status::{NetworkNoteStatus, NetworkNoteStatusInfo, RpcStatusInfo};
pub use endpoint::Endpoint;

#[cfg(not(feature = "testing"))]
mod generated;
#[cfg(feature = "testing")]
pub mod generated;

#[cfg(feature = "tonic")]
mod tonic_client;
#[cfg(feature = "tonic")]
pub use tonic_client::GrpcClient;

use crate::rpc::domain::account_vault::AccountVaultInfo;
use crate::rpc::domain::transaction::TransactionRecord;
use crate::store::InputNoteRecord;
use crate::store::input_note_states::UnverifiedNoteState;

/// Represents the state that we want to retrieve from the network
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AccountStateAt {
    /// Gets the latest state, for the current chain tip
    #[default]
    ChainTip,
    /// Gets the state at a specific block number
    Block(BlockNumber),
}

/// Returns `true` if the note's metadata advertises at least one attachment.
///
/// Sync records carry attachment scheme markers (not the attachment content), so a present scheme
/// in any header slot indicates the note has attachments whose content must be fetched separately.
fn metadata_has_attachments(metadata: &NoteMetadata) -> bool {
    metadata.attachment_headers().iter().any(|header| header.scheme().is_some())
}

// NODE RPC CLIENT TRAIT
// ================================================================================================

/// Defines the interface for communicating with the Miden node.
///
/// The implementers are responsible for connecting to the Miden node, handling endpoint
/// requests/responses, and translating responses into domain objects relevant for each of the
/// endpoints.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait NodeRpcClient: Send + Sync {
    /// Sets the genesis commitment for the client and reconnects to the node providing the
    /// genesis commitment in the request headers. If the genesis commitment is already set,
    /// this method does nothing.
    async fn set_genesis_commitment(&self, commitment: Word) -> Result<(), RpcError>;

    /// Returns the genesis commitment if it has been set, without fetching from the node.
    fn has_genesis_commitment(&self) -> Option<Word>;

    /// Given a Proven Transaction, send it to the node for it to be included in a future block
    /// using the `/SubmitProvenTransaction` RPC endpoint.
    ///
    /// Returns the node's chain tip at submission (not the block the transaction is committed in).
    async fn submit_proven_transaction(
        &self,
        proven_transaction: ProvenTransaction,
        transaction_inputs: TransactionInputs,
    ) -> Result<BlockNumber, RpcError>;

    /// Given a Proven Batch together with the corresponding [`ProposedBatch`] and the list of
    /// [`TransactionInputs`] (one per transaction, matching the ordering of the batch), sends
    /// the batch to the node for inclusion in a future block using the `/SubmitProvenBatch`
    /// RPC endpoint. All transactions in the batch must build on the current mempool state
    /// following normal transaction submission rules.
    ///
    /// Returns the node's chain tip at submission (not the block the batch is committed in).
    async fn submit_proven_batch(
        &self,
        proven_batch: ProvenBatch,
        proposed_batch: ProposedBatch,
        transaction_inputs: Vec<TransactionInputs>,
    ) -> Result<BlockNumber, RpcError>;

    /// Given a block number, fetches the block header corresponding to that height from the node
    /// using the `/GetBlockHeaderByNumber` endpoint.
    /// If `include_mmr_proof` is set to true and the function returns an `Ok`, the second value
    /// of the return tuple should always be Some(MmrProof).
    ///
    /// When `None` is provided, returns info regarding the latest block.
    ///
    /// When `block_num` is `Some`, implementations must verify that the returned header's block
    /// number matches the requested one and return [`RpcError::InvalidResponse`] otherwise.
    ///
    /// Errors:
    /// - [`RpcError::InvalidResponse`] if the node returns a header whose block number does not
    ///   match the requested `block_num`.
    async fn get_block_header_by_number(
        &self,
        block_num: Option<BlockNumber>,
        include_mmr_proof: bool,
    ) -> Result<(BlockHeader, Option<MmrProof>), RpcError>;

    /// Given a block number, fetches the block corresponding to that height from the node using
    /// the `/GetBlockByNumber` RPC endpoint.
    ///
    /// If `include_proof` is set to true, the block proof will be included in the response.
    ///
    /// Implementations must verify that the returned block's number matches the requested
    /// `block_num` and return [`RpcError::InvalidResponse`] otherwise.
    ///
    /// # Errors:
    /// - [`RpcError::InvalidResponse`] if the node returns a block whose number does not match the
    ///   requested `block_num`.
    async fn get_block_by_number(
        &self,
        block_num: BlockNumber,
        include_proof: bool,
    ) -> Result<ProvenBlock, RpcError>;

    /// Fetches note-related data for a list of [`NoteId`] using the `/GetNotesById`
    /// RPC endpoint.
    ///
    /// For [`miden_protocol::note::NoteType::Private`] notes, the response includes only the
    /// [`miden_protocol::note::NoteMetadata`].
    ///
    /// For [`miden_protocol::note::NoteType::Public`] notes, the response includes all note details
    /// (recipient, assets, script, etc.).
    ///
    /// In both cases, a [`miden_protocol::note::NoteInclusionProof`] is returned so the caller can
    /// verify that each note is part of the block's note tree.
    ///
    /// Implementations must verify that every returned note's ID was present in `note_ids` and
    /// return [`RpcError::InvalidResponse`] otherwise.
    ///
    /// Errors:
    /// - [`RpcError::InvalidResponse`] if the node returns a note whose ID was not requested.
    async fn get_notes_by_id(&self, note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError>;

    /// Fetches the MMR delta for a given block range using the `/SyncChainMmr` RPC endpoint.
    ///
    /// - `current_block_height` is the last block number already present in the caller's MMR.
    /// - `upper_bound` determines the upper bound of the sync range. Can be a specific block number
    ///   (`BlockNumber`), or a chain tip finality level: `CommittedChainTip` syncs up to the latest
    ///   committed block (the chain tip), while `ProvenChainTip` syncs up to the latest proven
    ///   block which may be behind the committed tip.
    async fn sync_chain_mmr(
        &self,
        current_block_height: BlockNumber,
        upper_bound: SyncTarget,
    ) -> Result<ChainMmrInfo, RpcError>;

    /// Fetches the full state of a public account from the node using the `/GetAccount` endpoint,
    /// and then resolves oversized vault and storage map entries via the `SyncVault` and
    /// `SyncStorageMap` endpoints when needed.
    ///
    /// - `account_id` is the ID of the wanted account.
    ///
    /// Returns `Ok(None)` for accounts without public state.
    async fn get_account_details(
        &self,
        account_id: AccountId,
    ) -> Result<Option<Account>, RpcError> {
        // Accounts without public state have no full state to fetch; only a commitment is on-chain.
        if !account_id.is_public() {
            return Ok(None);
        }

        // A single request fetches the full public state: every storage map's entries plus the
        // vault, with the storage layout discovered server-side.
        let (block_number, mut proof) = self
            .get_account(
                account_id,
                GetAccountRequest::new()
                    .with_storage(StorageMapFetch::All)
                    .with_vault(VaultFetch::Always),
            )
            .await?;

        if let Some(details) = proof.details_mut() {
            self.resolve_oversize_vault(account_id, block_number, details).await?;
            self.resolve_oversize_storage_maps(account_id, block_number, details).await?;
        }

        let details = proof.into_details().ok_or(RpcError::ExpectedDataMissing(
            "public account returned without details".into(),
        ))?;

        Ok(Some(Account::try_from(&details)?))
    }

    /// Fetches notes related to the specified tags using the `/SyncNotes` RPC endpoint,
    /// paginating over the full block range and returning, in block-number order, every block in
    /// that range that contains at least one note matching the requested tags.
    ///
    /// - `block_from`: The starting block number for the range (inclusive).
    /// - `block_to`: The ending block number for the range (inclusive).
    /// - `note_tags` is the set of tags used to filter the notes the client is interested in.
    ///
    /// Notes with attachments will have header-only metadata after this call; use
    /// [`NodeRpcClient::sync_notes_with_details`] to also resolve their full metadata and
    /// fetch public note bodies in a single follow-up call.
    ///
    /// Implementations must verify that every returned note's tag was present in `note_tags` and
    /// return [`RpcError::InvalidResponse`] otherwise.
    ///
    /// # Errors
    /// - [`RpcError::InvalidResponse`] if the node returns a note whose tag was not requested.
    async fn sync_notes(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<NoteSyncBlock>, RpcError>;

    /// Calls [`NodeRpcClient::sync_notes`] for the requested range, then makes a single
    /// [`NodeRpcClient::get_notes_by_id`] call to fetch full note bodies (scripts, assets,
    /// recipient) for public notes and attachment content for private notes that carry
    /// attachments.
    ///
    /// All public notes in the range are fetched (not just the ones the client tracks) to
    /// avoid revealing which specific notes the client is interested in. Private notes are only
    /// fetched when their synced metadata indicates non-empty attachments, since the sync record
    /// carries attachment scheme markers but not the attachment content, which is needed to
    /// reconstruct the note's ID.
    ///
    /// Returns the resolved note blocks paired with a map of the fetched content (public note
    /// bodies and private-note attachments), keyed by note ID.
    async fn sync_notes_with_details(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<(Vec<NoteSyncBlock>, BTreeMap<NoteId, SyncedNoteDetails>), RpcError> {
        let blocks = self.sync_notes(block_from, block_to, note_tags).await?;

        let note_ids: Vec<NoteId> = blocks
            .iter()
            .flat_map(|b| b.notes.values())
            .filter(|n| n.note_type() == NoteType::Public || metadata_has_attachments(n.metadata()))
            .map(|n| *n.note_id())
            .collect();

        let mut synced_notes: BTreeMap<NoteId, SyncedNoteDetails> = BTreeMap::new();

        if !note_ids.is_empty() {
            let fetched = self.get_notes_by_id(&note_ids).await?;

            for fetched_note in fetched {
                match fetched_note {
                    FetchedNote::Public(note, _) => {
                        synced_notes.insert(note.id(), SyncedNoteDetails::Public(note));
                    },
                    FetchedNote::Private(note_id, _, attachments, _) => {
                        let attachments = (!attachments.is_empty()).then_some(attachments);
                        synced_notes.insert(note_id, SyncedNoteDetails::Private(attachments));
                    },
                }
            }
        }

        Ok((blocks, synced_notes))
    }

    /// Fetches the nullifiers corresponding to a list of prefixes using the
    /// `/SyncNullifiers` RPC endpoint.
    ///
    /// - `prefix` is a list of nullifiers prefixes to search for.
    /// - `block_from`: The starting block number for the range (inclusive).
    /// - `block_to`: The ending block number for the range (inclusive).
    ///
    /// Implementations must verify that every returned nullifier's prefix was present in `prefix`
    /// and return [`RpcError::InvalidResponse`] otherwise.
    ///
    /// # Errors
    /// - [`RpcError::InvalidResponse`] if the node returns a nullifier whose prefix was not
    ///   requested.
    async fn sync_nullifiers(
        &self,
        prefix: &[u16],
        block_from: BlockNumber,
        block_to: BlockNumber,
    ) -> Result<Vec<NullifierUpdate>, RpcError>;

    /// Fetches the account from the node, using the `/GetAccount` endpoint.
    ///
    /// The response carries an
    /// [`AccountWitness`](miden_protocol::block::account_tree::AccountWitness) and the target
    /// block. Public accounts additionally get [`AccountDetails`]; for private accounts the
    /// other `request` fields are ignored.
    ///
    /// For a fully oversize-resolved account, use [`NodeRpcClient::get_account_details`].
    ///
    /// Errors if the account isn't found or the block number of the requested account doesn't match
    /// # Errors
    ///
    /// - If the account isn't found in the network
    /// - If the response block number does not match the requested one
    async fn get_account(
        &self,
        account_id: AccountId,
        request: GetAccountRequest,
    ) -> Result<(BlockNumber, AccountProof), RpcError>;

    /// Fills in the asset list when the vault came back flagged `too_many_assets`, by
    /// querying [`NodeRpcClient::sync_account_vault`] over `[GENESIS, block_to]`. No-op when
    /// the flag isn't set.
    async fn resolve_oversize_vault(
        &self,
        account_id: AccountId,
        block_to: BlockNumber,
        details: &mut AccountDetails,
    ) -> Result<(), RpcError> {
        if !details.vault_details.too_many_assets {
            return Ok(());
        }
        let vault_info =
            self.sync_account_vault(BlockNumber::GENESIS, block_to, account_id).await?;
        let mut updates = vault_info.updates;
        // The node returns the full history of vault entries, so a given key may appear in more
        // than one block. Sort by block so the BTreeMap keeps the latest value per key.
        updates.sort_by_key(|u| u.block_num);
        details.vault_details.assets = updates
            .into_iter()
            .map(|u| (u.vault_key, u.asset))
            .collect::<BTreeMap<_, _>>()
            .into_values()
            .flatten()
            .collect();
        details.vault_details.too_many_assets = false;
        Ok(())
    }

    /// Fills in the entries of any storage map flagged `too_many_entries`, by querying
    /// [`NodeRpcClient::sync_storage_maps`] over `[GENESIS, block_to]`. No-op when no map
    /// has the flag set.
    async fn resolve_oversize_storage_maps(
        &self,
        account_id: AccountId,
        block_to: BlockNumber,
        details: &mut AccountDetails,
    ) -> Result<(), RpcError> {
        if !details.storage_details.map_details.iter().any(|m| m.too_many_entries) {
            return Ok(());
        }
        let info = self.sync_storage_maps(BlockNumber::GENESIS, block_to, account_id).await?;
        for map_details in &mut details.storage_details.map_details {
            if !map_details.too_many_entries {
                continue;
            }
            // The node returns the full history of map entries, so a given key may appear in
            // more than one block. Sort by block so the BTreeMap keeps the latest value per key.
            let mut sorted: Vec<_> =
                info.updates.iter().filter(|u| u.slot_name == map_details.slot_name).collect();
            sorted.sort_by_key(|u| u.block_num);
            let entries: Vec<StorageMapEntry> = sorted
                .into_iter()
                .map(|u| (u.key, u.value))
                .collect::<BTreeMap<_, _>>()
                .into_iter()
                .map(|(key, value)| StorageMapEntry { key, value })
                .collect();
            map_details.too_many_entries = false;
            map_details.entries = StorageMapEntries::AllEntries(entries);
        }
        Ok(())
    }

    /// Fetches the commit height where the nullifier was consumed. If the nullifier isn't found,
    /// then `None` is returned.
    /// The `block_num` parameter is the block number to start the search from (inclusive).
    ///
    /// The default implementation of this method makes two RPC requests: one to
    /// [`NodeRpcClient::get_block_header_by_number`] to resolve the chain tip, and one to
    /// [`NodeRpcClient::sync_nullifiers`] to search up to that tip.
    async fn get_nullifier_commit_heights(
        &self,
        requested_nullifiers: BTreeSet<Nullifier>,
        block_from: BlockNumber,
    ) -> Result<BTreeMap<Nullifier, Option<BlockNumber>>, RpcError> {
        let prefixes: Vec<u16> =
            requested_nullifiers.iter().map(crate::note::Nullifier::prefix).collect();
        let (chain_tip, _) = self.get_block_header_by_number(None, false).await?;
        let retrieved_nullifiers =
            self.sync_nullifiers(&prefixes, block_from, chain_tip.block_num()).await?;

        let mut nullifiers_height = BTreeMap::new();
        for nullifier in requested_nullifiers {
            if let Some(update) =
                retrieved_nullifiers.iter().find(|update| update.nullifier == nullifier)
            {
                nullifiers_height.insert(nullifier, Some(update.block_num));
            } else {
                nullifiers_height.insert(nullifier, None);
            }
        }

        Ok(nullifiers_height)
    }

    /// Fetches public note-related data for a list of [`NoteId`] and builds [`InputNoteRecord`]s
    /// with it. If a note is not found or it's private, it is ignored and will not be included
    /// in the returned list.
    ///
    /// The default implementation of this method uses [`NodeRpcClient::get_notes_by_id`].
    async fn get_public_note_records(
        &self,
        note_ids: &[NoteId],
        current_timestamp: Option<u64>,
    ) -> Result<Vec<InputNoteRecord>, RpcError> {
        if note_ids.is_empty() {
            return Ok(vec![]);
        }

        let mut public_notes = Vec::with_capacity(note_ids.len());
        let note_details = self.get_notes_by_id(note_ids).await?;

        for detail in note_details {
            if let FetchedNote::Public(note, inclusion_proof) = detail {
                let state = UnverifiedNoteState {
                    metadata: *note.metadata(),
                    inclusion_proof,
                }
                .into();
                let attachments = note.attachments().clone();
                let note = InputNoteRecord::new(note.into(), attachments, current_timestamp, state);

                public_notes.push(note);
            }
        }

        Ok(public_notes)
    }

    /// Given a block number, fetches the block header corresponding to that height from the node
    /// along with the MMR proof.
    ///
    /// The default implementation of this method uses
    /// [`NodeRpcClient::get_block_header_by_number`].
    async fn get_block_header_with_proof(
        &self,
        block_num: BlockNumber,
    ) -> Result<(BlockHeader, MmrProof), RpcError> {
        let (header, proof) = self.get_block_header_by_number(Some(block_num), true).await?;
        Ok((header, proof.ok_or(RpcError::ExpectedDataMissing(String::from("MmrProof")))?))
    }

    /// Fetches the note with the specified ID.
    ///
    /// The default implementation of this method uses [`NodeRpcClient::get_notes_by_id`].
    ///
    /// Errors:
    /// - [`RpcError::NoteNotFound`] if the note with the specified ID is not found.
    async fn get_note_by_id(&self, note_id: NoteId) -> Result<FetchedNote, RpcError> {
        let notes = self.get_notes_by_id(&[note_id]).await?;
        notes.into_iter().next().ok_or(RpcError::NoteNotFound(note_id))
    }

    /// Fetches the note script with the specified root, returning `None` if the node has no script
    /// registered for that root.
    ///
    /// Implementations must verify that a returned script's root matches the requested `root` and
    /// return [`RpcError::InvalidResponse`] otherwise; callers may rely on this invariant.
    ///
    /// Errors:
    /// - [`RpcError::InvalidResponse`] if the node returns a script whose root does not match the
    ///   requested `root`.
    async fn get_note_script_by_root(&self, root: Word) -> Result<Option<NoteScript>, RpcError>;

    /// Fetches storage map updates for specified account and storage slots within a block range,
    /// using the `/SyncStorageMaps` RPC endpoint.
    ///
    /// - `block_from`: The starting block number for the range (inclusive).
    /// - `block_to`: The ending block number for the range (inclusive). The node rejects values
    ///   greater than the chain tip.
    /// - `account_id`: The account ID for which to fetch storage map updates.
    async fn sync_storage_maps(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<StorageMapInfo, RpcError>;

    /// Fetches account vault updates for specified account within a block range,
    /// using the `/SyncAccountVault` RPC endpoint.
    ///
    /// - `block_from`: The starting block number for the range (inclusive).
    /// - `block_to`: The ending block number for the range (inclusive). The node rejects values
    ///   greater than the chain tip.
    /// - `account_id`: The account ID for which to fetch storage map updates.
    async fn sync_account_vault(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<AccountVaultInfo, RpcError>;

    /// Fetches transaction records for specific accounts within a block range using the
    /// `/SyncTransactions` RPC endpoint.
    ///
    /// - `block_from`: The starting block number for the range (inclusive).
    /// - `block_to`: The ending block number for the range (inclusive).
    /// - `account_ids`: The account IDs for which to fetch transactions.
    async fn sync_transactions(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: Vec<AccountId>,
    ) -> Result<Vec<TransactionRecord>, RpcError>;

    /// Fetches the network ID of the node.
    /// Errors:
    /// - [`RpcError::ExpectedDataMissing`] if the note with the specified root is not found.
    async fn get_network_id(&self) -> Result<NetworkId, RpcError>;

    /// Fetches the RPC limits configured on the node.
    ///
    /// Implementations may cache the result internally to avoid repeated network calls.
    async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError>;

    /// Returns the RPC limits if they have been set, without fetching from the node.
    fn has_rpc_limits(&self) -> Option<RpcLimits>;

    /// Sets the RPC limits internally to be used by the client.
    async fn set_rpc_limits(&self, limits: RpcLimits);

    /// Fetches the RPC status without requiring Accept header validation.
    ///
    /// This is useful for diagnostics when version negotiation fails, as it allows
    /// retrieving node information even when there's a version mismatch.
    async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError>;

    /// Fetches the status of a specific network note ID.
    ///
    /// This is useful for debugging when a network note fails.
    async fn get_network_note_status(
        &self,
        note_id: NoteId,
    ) -> Result<NetworkNoteStatusInfo, RpcError>;
}

// RPC API ENDPOINT
// ================================================================================================
//
/// RPC methods for the Miden protocol.
#[derive(Debug, Clone, Copy)]
pub enum RpcEndpoint {
    Status,
    SyncNullifiers,
    GetAccount,
    GetBlockByNumber,
    GetBlockHeaderByNumber,
    GetNotesById,
    SyncChainMmr,
    SubmitProvenTx,
    SubmitProvenBatch,
    SyncNotes,
    GetNoteScriptByRoot,
    SyncStorageMaps,
    SyncAccountVault,
    SyncTransactions,
    GetLimits,
    GetNetworkNoteStatus,
}

impl RpcEndpoint {
    /// Returns the endpoint name as used in the RPC service definition.
    pub fn proto_name(&self) -> &'static str {
        match self {
            RpcEndpoint::Status => "Status",
            RpcEndpoint::SyncNullifiers => "SyncNullifiers",
            RpcEndpoint::GetAccount => "GetAccount",
            RpcEndpoint::GetBlockByNumber => "GetBlockByNumber",
            RpcEndpoint::GetBlockHeaderByNumber => "GetBlockHeaderByNumber",
            RpcEndpoint::GetNotesById => "GetNotesById",
            RpcEndpoint::SyncChainMmr => "SyncChainMmr",
            RpcEndpoint::SubmitProvenTx => "SubmitProvenTransaction",
            RpcEndpoint::SubmitProvenBatch => "SubmitProvenBatch",
            RpcEndpoint::SyncNotes => "SyncNotes",
            RpcEndpoint::GetNoteScriptByRoot => "GetNoteScriptByRoot",
            RpcEndpoint::SyncStorageMaps => "SyncStorageMaps",
            RpcEndpoint::SyncAccountVault => "SyncAccountVault",
            RpcEndpoint::SyncTransactions => "SyncTransactions",
            RpcEndpoint::GetLimits => "GetLimits",
            RpcEndpoint::GetNetworkNoteStatus => "GetNetworkNoteStatus",
        }
    }
}

impl fmt::Display for RpcEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RpcEndpoint::Status => write!(f, "status"),
            RpcEndpoint::SyncNullifiers => {
                write!(f, "sync_nullifiers")
            },
            RpcEndpoint::GetAccount => write!(f, "get_account"),
            RpcEndpoint::GetBlockByNumber => write!(f, "get_block_by_number"),
            RpcEndpoint::GetBlockHeaderByNumber => {
                write!(f, "get_block_header_by_number")
            },
            RpcEndpoint::GetNotesById => write!(f, "get_notes_by_id"),
            RpcEndpoint::SyncChainMmr => write!(f, "sync_chain_mmr"),
            RpcEndpoint::SubmitProvenTx => write!(f, "submit_proven_transaction"),
            RpcEndpoint::SubmitProvenBatch => write!(f, "submit_proven_batch"),
            RpcEndpoint::SyncNotes => write!(f, "sync_notes"),
            RpcEndpoint::GetNoteScriptByRoot => write!(f, "get_note_script_by_root"),
            RpcEndpoint::SyncStorageMaps => write!(f, "sync_storage_maps"),
            RpcEndpoint::SyncAccountVault => write!(f, "sync_account_vault"),
            RpcEndpoint::SyncTransactions => write!(f, "sync_transactions"),
            RpcEndpoint::GetLimits => write!(f, "get_limits"),
            RpcEndpoint::GetNetworkNoteStatus => write!(f, "get_network_note_status"),
        }
    }
}

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{
    AccountId,
    AccountUpdateDetails,
    AccountVaultPatch,
    StorageMapPatchEntries,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
    StorageSlotType,
};
use miden_protocol::address::NetworkId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::{BlockHeader, BlockNumber, ProvenBlock};
use miden_protocol::crypto::merkle::mmr::{Forest, Mmr, MmrProof};
use miden_protocol::note::{NoteAttachments, NoteHeader, NoteId, NoteScript, NoteTag};
use miden_protocol::transaction::{ProvenTransaction, TransactionInputs};
use miden_testing::{MockChain, MockChainNote};
use miden_tx::utils::sync::RwLock;

use crate::Client;
use crate::rpc::domain::account::{
    AccountDetails,
    AccountProof,
    AccountStorageDetails,
    AccountStorageMapDetails,
    AccountVaultDetails,
    GetAccountRequest,
    StorageMapEntries,
    StorageMapEntry,
    StorageMapFetch,
};
use crate::rpc::domain::account_vault::AccountVaultInfo;
use crate::rpc::domain::note::{CommittedNote, FetchedNote, NoteSyncBlock};
use crate::rpc::domain::nullifier::NullifierUpdate;
use crate::rpc::domain::status::NetworkNoteStatusInfo;
use crate::rpc::domain::storage_map::StorageMapInfo;
use crate::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
use crate::rpc::domain::transaction::TransactionRecord;
use crate::rpc::{AccountStateAt, NodeRpcClient, RpcError, RpcStatusInfo};

pub type MockClient<AUTH> = Client<AUTH>;

/// Mock RPC API
///
/// This struct implements the RPC API used by the client to communicate with the node. It simulates
/// most of the functionality of the actual node, with some small differences:
/// - It uses a [`MockChain`] to simulate the blockchain state.
/// - Blocks are not automatically created after time passes, but rather new blocks are created when
///   calling the `prove_block` method.
/// - Network account and transactions aren't supported in the current version.
/// - Account update block numbers aren't tracked, so any endpoint that returns when certain account
///   updates were made will return the chain tip block number instead.
#[derive(Clone)]
pub struct MockRpcApi {
    account_commitment_updates: Arc<RwLock<BTreeMap<BlockNumber, BTreeMap<AccountId, Word>>>>,
    pub mock_chain: Arc<RwLock<MockChain>>,
    oversize_threshold: usize,
    /// Note headers to report as erased in sync transaction responses.
    erased_notes: Arc<RwLock<Vec<NoteHeader>>>,
    /// Attachment content for private notes, keyed by note ID. The [`MockChain`] stores private
    /// notes without their attachment content (only metadata), so tests that need
    /// `get_notes_by_id` to return private-note attachments register them here.
    private_note_attachments: Arc<RwLock<BTreeMap<NoteId, NoteAttachments>>>,
}

impl Default for MockRpcApi {
    fn default() -> Self {
        Self::new(MockChain::new())
    }
}

impl MockRpcApi {
    // Constant to use in mocked pagination.
    const PAGINATION_BLOCK_LIMIT: u32 = 5;

    /// Creates a new [`MockRpcApi`] instance with the state of the provided [`MockChain`].
    pub fn new(mock_chain: MockChain) -> Self {
        Self {
            account_commitment_updates: Arc::new(RwLock::new(build_account_updates(&mock_chain))),
            mock_chain: Arc::new(RwLock::new(mock_chain)),
            oversize_threshold: 1000,
            erased_notes: Arc::new(RwLock::new(Vec::new())),
            private_note_attachments: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Registers the attachment content for a private note so that subsequent `get_notes_by_id`
    /// responses include it, mirroring a node that stores private-note attachments on-chain.
    pub fn register_private_note_attachments(&self, note_id: NoteId, attachments: NoteAttachments) {
        self.private_note_attachments.write().insert(note_id, attachments);
    }

    /// Sets the oversize threshold for `get_account`. Any storage map with more entries than
    /// this threshold, or a vault with more assets, will have the `too_many_entries` /
    /// `too_many_assets` flags set in the response.
    #[must_use]
    pub fn with_oversize_threshold(mut self, threshold: usize) -> Self {
        self.oversize_threshold = threshold;
        self
    }

    /// Registers a note header to be reported as erased in subsequent sync transaction responses.
    pub fn mark_note_as_erased(&self, header: NoteHeader) {
        self.erased_notes.write().push(header);
    }

    /// Returns the current MMR of the blockchain.
    pub fn get_mmr(&self) -> Mmr {
        self.mock_chain.read().blockchain().as_mmr().clone()
    }

    /// Returns the chain tip block number.
    pub fn get_chain_tip_block_num(&self) -> BlockNumber {
        self.mock_chain.read().latest_block_header().block_num()
    }

    /// Advances the mock chain by proving the next block, committing all pending objects to the
    /// chain in the process.
    pub fn prove_block(&self) {
        let proven_block = self.mock_chain.write().prove_next_block().unwrap();
        let mut account_commitment_updates = self.account_commitment_updates.write();
        let block_num = proven_block.header().block_num();
        let updates: BTreeMap<AccountId, Word> = proven_block
            .body()
            .updated_accounts()
            .iter()
            .map(|update| (update.account_id(), update.final_state_commitment()))
            .collect();

        if !updates.is_empty() {
            account_commitment_updates.insert(block_num, updates);
        }
    }

    /// Retrieves a block by its block number.
    fn get_block_by_num(&self, block_num: BlockNumber) -> BlockHeader {
        self.mock_chain.read().block_header(block_num.as_usize())
    }

    /// Retrieves account vault updates in a given block range.
    /// This method tries to simulate pagination by limiting the number of blocks processed per
    /// request.
    fn get_sync_account_vault_request(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> (BlockNumber, BlockNumber, AccountVaultPatch) {
        let chain_tip = self.get_chain_tip_block_num();
        let target_block = block_to.min(chain_tip);

        let page_end_block: BlockNumber = (block_from.as_u32() + Self::PAGINATION_BLOCK_LIMIT)
            .min(target_block.as_u32())
            .into();

        // Blocks are iterated in ascending order, so later blocks win per asset ID.
        let mut vault_patch = AccountVaultPatch::default();
        for block in self.mock_chain.read().proven_blocks() {
            let block_number = block.header().block_num();
            // Only include blocks in range [block_from, page_end_block]
            if block_number < block_from || block_number > page_end_block {
                continue;
            }

            for update in block
                .body()
                .updated_accounts()
                .iter()
                .filter(|block_acc_update| block_acc_update.account_id() == account_id)
            {
                let AccountUpdateDetails::Public(patch) = update.details().clone() else {
                    continue;
                };

                vault_patch.merge(patch.vault().clone());
            }
        }

        (chain_tip, page_end_block, vault_patch)
    }

    /// Retrieves transactions in a given block range that match the provided account IDs
    fn get_sync_transactions_request(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: &[AccountId],
    ) -> Vec<TransactionRecord> {
        let mut transactions = Vec::new();
        for block in self.mock_chain.read().proven_blocks() {
            let block_number = block.header().block_num();
            if block_number < block_from || block_number > block_to {
                continue;
            }

            for transaction_header in block.body().transactions().as_slice() {
                if !account_ids.contains(&transaction_header.account_id()) {
                    continue;
                }

                let erased_output_notes = self.erased_notes.read().clone();

                transactions.push(TransactionRecord {
                    block_num: block_number,
                    transaction_header: transaction_header.clone(),
                    output_notes: vec![],
                    erased_output_notes,
                });
            }
        }

        transactions
    }

    /// Retrieves storage map updates in a given block range.
    ///
    /// This method tries to simulate pagination of the real node.
    fn get_sync_storage_maps_request(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> (BlockNumber, BlockNumber, BTreeMap<StorageSlotName, StorageMapPatchEntries>) {
        let chain_tip = self.get_chain_tip_block_num();
        let target_block = block_to.min(chain_tip);

        let page_end_block: BlockNumber = (block_from.as_u32() + Self::PAGINATION_BLOCK_LIMIT)
            .min(target_block.as_u32())
            .into();

        // Blocks are iterated in ascending order, so later blocks win per `(slot, key)`.
        let mut map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries> = BTreeMap::new();
        for block in self.mock_chain.read().proven_blocks() {
            let block_number = block.header().block_num();
            // Only include blocks in range [block_from, page_end_block]
            if block_number < block_from || block_number > page_end_block {
                continue;
            }

            for update in block
                .body()
                .updated_accounts()
                .iter()
                .filter(|block_acc_update| block_acc_update.account_id() == account_id)
            {
                let AccountUpdateDetails::Public(patch) = update.details().clone() else {
                    continue;
                };

                for (slot_name, map_patch) in patch.storage().maps() {
                    if let Some(entries) = map_patch.entries() {
                        map_entries
                            .entry(slot_name.clone())
                            .or_default()
                            .as_map_mut()
                            .extend(entries.as_map().clone());
                    }
                }
            }
        }

        (chain_tip, page_end_block, map_entries)
    }

    pub fn get_available_notes(&self) -> Vec<MockChainNote> {
        self.mock_chain.read().committed_notes().values().cloned().collect()
    }

    pub fn get_public_available_notes(&self) -> Vec<MockChainNote> {
        self.mock_chain
            .read()
            .committed_notes()
            .values()
            .filter(|n| matches!(n, MockChainNote::Public(_, _)))
            .cloned()
            .collect()
    }

    pub fn get_private_available_notes(&self) -> Vec<MockChainNote> {
        self.mock_chain
            .read()
            .committed_notes()
            .values()
            .filter(|n| matches!(n, MockChainNote::Private(_, _, _, _)))
            .cloned()
            .collect()
    }

    pub fn advance_blocks(&self, num_blocks: u32) {
        let current_height = self.get_chain_tip_block_num();
        let mut mock_chain = self.mock_chain.write();
        mock_chain.prove_until_block(current_height + num_blocks).unwrap();
    }
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl NodeRpcClient for MockRpcApi {
    fn has_genesis_commitment(&self) -> Option<Word> {
        None
    }

    async fn set_genesis_commitment(&self, _commitment: Word) -> Result<(), RpcError> {
        // The mock client doesn't use accept headers, so we don't need to do anything here.
        Ok(())
    }

    /// Returns note updates in the inclusive block range `[block_from, block_to]`.
    /// Only notes that match the provided tags will be returned, grouped by block.
    async fn sync_notes(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<NoteSyncBlock>, RpcError> {
        let mut blocks_with_notes: BTreeMap<BlockNumber, BTreeMap<NoteId, CommittedNote>> =
            BTreeMap::new();
        for note in self.mock_chain.read().committed_notes().values() {
            let note_block = note.inclusion_proof().location().block_num();
            if note_tags.contains(&note.metadata().tag())
                && note_block >= block_from
                && note_block <= block_to
            {
                let committed =
                    CommittedNote::new(note.id(), *note.metadata(), note.inclusion_proof().clone());
                blocks_with_notes.entry(note_block).or_default().insert(note.id(), committed);
            }
        }

        Ok(blocks_with_notes
            .into_iter()
            .map(|(bn, notes)| {
                let block_header = self.get_block_by_num(bn);
                let mmr_path = self.get_mmr().open(bn.as_usize()).unwrap().merkle_path().clone();
                NoteSyncBlock { block_header, mmr_path, notes }
            })
            .collect())
    }

    async fn sync_chain_mmr(
        &self,
        current_block_height: BlockNumber,
        upper_bound: SyncTarget,
    ) -> Result<ChainMmrInfo, RpcError> {
        let chain_tip = self.get_chain_tip_block_num();
        // The mock chain doesn't distinguish committed vs proven tips.
        let target_block = match upper_bound {
            SyncTarget::CommittedChainTip | SyncTarget::ProvenChainTip => chain_tip,
        };

        let from_forest = if current_block_height == target_block {
            target_block.as_usize()
        } else {
            current_block_height.as_u32() as usize + 1
        };

        let mmr_delta = self
            .get_mmr()
            .get_delta(
                Forest::new(from_forest).unwrap(),
                Forest::new(target_block.as_usize()).unwrap(),
            )
            .unwrap();

        let block_header = self.get_block_by_num(target_block);

        Ok(ChainMmrInfo {
            block_from: current_block_height,
            block_to: target_block,
            mmr_delta,
            block_header,
        })
    }

    /// Retrieves the block header for the specified block number. If the block number is not
    /// provided, the chain tip block header will be returned.
    async fn get_block_header_by_number(
        &self,
        block_num: Option<BlockNumber>,
        include_mmr_proof: bool,
    ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
        let block = if let Some(block_num) = block_num {
            self.mock_chain.read().block_header(block_num.as_usize())
        } else {
            self.mock_chain.read().latest_block_header()
        };

        let mmr_proof = if include_mmr_proof {
            Some(self.get_mmr().open(block_num.unwrap().as_usize()).unwrap())
        } else {
            None
        };

        Ok((block, mmr_proof))
    }

    /// Returns the node's tracked notes that match the provided note IDs.
    async fn get_notes_by_id(&self, note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError> {
        // assume all public notes for now
        let notes = self.mock_chain.read().committed_notes().clone();

        let hit_notes = note_ids.iter().filter_map(|id| notes.get(id));
        let mut return_notes = vec![];
        for note in hit_notes {
            let fetched_note = match note {
                MockChainNote::Private(note_id, note_metadata, _, note_inclusion_proof) => {
                    let attachments = self
                        .private_note_attachments
                        .read()
                        .get(note_id)
                        .cloned()
                        .unwrap_or_else(NoteAttachments::empty);
                    FetchedNote::Private(
                        *note_id,
                        *note_metadata,
                        attachments,
                        note_inclusion_proof.clone(),
                    )
                },
                MockChainNote::Public(note, note_inclusion_proof) => {
                    FetchedNote::Public(note.clone(), note_inclusion_proof.clone())
                },
            };
            return_notes.push(fetched_note);
        }
        Ok(return_notes)
    }

    /// Simulates the submission of a proven transaction to the node. This will create a new block
    /// just for the new transaction and return the block number of the newly created block.
    async fn submit_proven_transaction(
        &self,
        proven_transaction: ProvenTransaction,
        _tx_inputs: TransactionInputs, // Unnecessary for testing client itself.
    ) -> Result<BlockNumber, RpcError> {
        // TODO: add some basic validations to test error cases

        {
            let mut mock_chain = self.mock_chain.write();
            mock_chain.add_pending_proven_transaction(proven_transaction.clone());
        };

        let block_num = self.get_chain_tip_block_num();

        Ok(block_num)
    }

    /// Simulates the submission of a proven batch to the node by adding it to the mock chain's
    /// pending batches. The `proposed_batch` and `transaction_inputs` arguments are accepted to
    /// match the trait signature but are unused — the mock relies on the `ProvenBatch` alone,
    /// matching how `submit_proven_transaction` ignores its `transaction_inputs`.
    async fn submit_proven_batch(
        &self,
        proven_batch: ProvenBatch,
        _proposed_batch: ProposedBatch,
        _transaction_inputs: Vec<TransactionInputs>,
    ) -> Result<BlockNumber, RpcError> {
        let mut mock_chain = self.mock_chain.write();
        mock_chain.add_pending_batch(proven_batch);
        drop(mock_chain);

        let block_num = self.get_chain_tip_block_num();

        Ok(block_num)
    }

    /// Returns the account proof for the specified account. The `known_code` and `vault` fields
    /// of the request are ignored in the mock implementation: the latest account code and full
    /// asset list are always returned, and the truncation flags are set when the data exceeds
    /// `oversize_threshold`.
    async fn get_account(
        &self,
        account_id: AccountId,
        request: GetAccountRequest,
    ) -> Result<(BlockNumber, AccountProof), RpcError> {
        let mock_chain = self.mock_chain.read();

        let block_number = match request.at {
            AccountStateAt::Block(number) => number,
            AccountStateAt::ChainTip => mock_chain.latest_block_header().block_num(),
        };

        let headers = if account_id.is_public() {
            let account = mock_chain.committed_account(account_id).unwrap();

            // `All` enumerates the account's map slots directly — the mock can introspect the
            // account, so it simulates the (not-yet-on-the-wire) "all storage maps" request.
            let requested_slots: Vec<_> = match &request.storage {
                StorageMapFetch::Skip => Vec::new(),
                StorageMapFetch::Slots(reqs) => reqs.inner().keys().cloned().collect(),
                StorageMapFetch::All => account
                    .storage()
                    .to_header()
                    .slots()
                    .filter(|slot| slot.slot_type() == StorageSlotType::Map)
                    .map(|slot| slot.name().clone())
                    .collect(),
            };

            let mut map_details = vec![];
            for slot_name in &requested_slots {
                if let Some(StorageSlotContent::Map(storage_map)) =
                    account.storage().get(slot_name).map(StorageSlot::content)
                {
                    let entries: Vec<StorageMapEntry> = storage_map
                        .entries()
                        .map(|(key, value)| StorageMapEntry { key: *key, value: *value })
                        .collect();

                    // NOTE: The mock returns all entries even when too_many_entries is set.
                    // In production, the node would return partial data for oversized maps.
                    let too_many_entries = entries.len() > self.oversize_threshold;
                    let account_storage_map_detail = AccountStorageMapDetails {
                        slot_name: slot_name.clone(),
                        too_many_entries,
                        entries: StorageMapEntries::AllEntries(entries),
                    };

                    map_details.push(account_storage_map_detail);
                } else {
                    panic!("Storage slot {slot_name} is not a map");
                }
            }

            let storage_details = AccountStorageDetails {
                header: account.storage().to_header(),
                map_details,
            };

            let mut assets = vec![];
            for asset in account.vault().assets() {
                assets.push(asset);
            }
            let vault_details = AccountVaultDetails {
                too_many_assets: assets.len() > self.oversize_threshold,
                assets,
            };

            Some(AccountDetails {
                header: account.into(),
                storage_details,
                code: account.code().clone(),
                vault_details,
            })
        } else {
            None
        };

        let witness = mock_chain.account_tree().open(account_id);

        let proof = AccountProof::new(witness, headers).unwrap();

        Ok((block_number, proof))
    }

    /// Returns the nullifiers created after the specified block number that match the provided
    /// prefixes.
    async fn sync_nullifiers(
        &self,
        prefixes: &[u16],
        block_from: BlockNumber,
        block_to: BlockNumber,
    ) -> Result<Vec<NullifierUpdate>, RpcError> {
        let nullifiers = self
            .mock_chain
            .read()
            .nullifier_tree()
            .entries()
            .filter_map(|(nullifier, block_num)| {
                let within_range = block_num >= block_from && block_num <= block_to;

                if prefixes.contains(&nullifier.prefix()) && within_range {
                    Some(NullifierUpdate { nullifier, block_num })
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        Ok(nullifiers)
    }

    async fn get_block_by_number(
        &self,
        block_num: BlockNumber,
        _include_proof: bool,
    ) -> Result<ProvenBlock, RpcError> {
        let block = self
            .mock_chain
            .read()
            .proven_blocks()
            .iter()
            .find(|b| b.header().block_num() == block_num)
            .unwrap()
            .clone();

        Ok(block)
    }

    async fn get_note_script_by_root(&self, root: Word) -> Result<Option<NoteScript>, RpcError> {
        let script = self
            .get_available_notes()
            .iter()
            .filter_map(|note| note.note())
            .find(|n| Word::from(n.script().root()) == root)
            .map(|n| n.script().clone());

        Ok(script)
    }

    async fn sync_storage_maps(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<StorageMapInfo, RpcError> {
        let mut map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries> = BTreeMap::new();
        let mut current_block_from = block_from;
        let chain_tip = self.get_chain_tip_block_num();
        let target_block = block_to.min(chain_tip);

        loop {
            let (page_chain_tip, page_block_number, page_entries) =
                self.get_sync_storage_maps_request(current_block_from, block_to, account_id);
            for (slot_name, entries) in page_entries {
                map_entries
                    .entry(slot_name)
                    .or_default()
                    .as_map_mut()
                    .extend(entries.into_map());
            }

            if page_block_number >= target_block {
                return Ok(StorageMapInfo {
                    chain_tip: page_chain_tip,
                    block_number: page_block_number,
                    map_entries,
                });
            }

            current_block_from = (page_block_number.as_u32() + 1).into();
        }
    }

    async fn sync_account_vault(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<AccountVaultInfo, RpcError> {
        let mut vault_patch = AccountVaultPatch::default();
        let mut current_block_from = block_from;
        let chain_tip = self.get_chain_tip_block_num();
        let target_block = block_to.min(chain_tip);

        loop {
            let (page_chain_tip, page_block_number, page_patch) =
                self.get_sync_account_vault_request(current_block_from, block_to, account_id);
            vault_patch.merge(page_patch);

            if page_block_number >= target_block {
                return Ok(AccountVaultInfo {
                    chain_tip: page_chain_tip,
                    block_number: page_block_number,
                    vault_patch,
                });
            }

            current_block_from = (page_block_number.as_u32() + 1).into();
        }
    }

    async fn sync_transactions(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: Vec<AccountId>,
    ) -> Result<Vec<TransactionRecord>, RpcError> {
        Ok(self.get_sync_transactions_request(block_from, block_to, &account_ids))
    }

    async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
        Ok(NetworkId::Testnet)
    }

    async fn get_rpc_limits(&self) -> Result<crate::rpc::RpcLimits, RpcError> {
        Ok(crate::rpc::RpcLimits::default())
    }

    fn has_rpc_limits(&self) -> Option<crate::rpc::RpcLimits> {
        None
    }

    async fn set_rpc_limits(&self, _limits: crate::rpc::RpcLimits) {
        // No-op for mock client
    }

    async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
        Ok(RpcStatusInfo {
            version: env!("CARGO_PKG_VERSION").into(),
            genesis_commitment: None,
            chain_tip: 0,
            block_producer: None,
        })
    }

    async fn get_network_note_status(
        &self,
        _note_id: NoteId,
    ) -> Result<NetworkNoteStatusInfo, RpcError> {
        todo!("We need to check if we want to implement this for the mockchain");
    }
}

// CONVERSIONS
// ================================================================================================

impl From<MockChain> for MockRpcApi {
    fn from(mock_chain: MockChain) -> Self {
        MockRpcApi::new(mock_chain)
    }
}

// HELPERS
// ================================================================================================

fn build_account_updates(
    mock_chain: &MockChain,
) -> BTreeMap<BlockNumber, BTreeMap<AccountId, Word>> {
    let mut account_commitment_updates = BTreeMap::new();
    for block in mock_chain.proven_blocks() {
        let block_num = block.header().block_num();
        let mut updates = BTreeMap::new();

        for update in block.body().updated_accounts() {
            updates.insert(update.account_id(), update.final_state_commitment());
        }

        if updates.is_empty() {
            continue;
        }

        account_commitment_updates.insert(block_num, updates);
    }
    account_commitment_updates
}

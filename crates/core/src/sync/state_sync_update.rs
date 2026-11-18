use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use miden_protocol::account::{Account, AccountHeader, AccountId, AccountPatch};
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, MmrPeaks};
use miden_protocol::protocol_config::ProtocolConfig;
use miden_protocol::transaction::TransactionId;
use miden_protocol::{Felt, Word};

use crate::store::NoteUpdates;
use crate::transaction::{TransactionRecord, TransactionStatus};

// STATE SYNC UPDATE
// ================================================================================================

/// Contains all information needed to apply the update in the store after syncing with the node.
///
/// Immutable once built: `StateSync::sync_state` in the client assembles the individual trackers
/// and seals them into this type at the end of the sync pass. Use [`Self::from_parts`] to build one
/// directly.
pub struct StateSyncUpdate {
    /// The block number of the last block that was synced.
    block_num: BlockNumber,
    /// New blocks, authentication nodes and MMR peaks.
    partial_blockchain_updates: PartialBlockchainUpdates,
    /// New and updated notes to be upserted in the store.
    note_updates: NoteUpdates,
    /// Committed and discarded transactions after the sync.
    transaction_updates: TransactionUpdates,
    /// Public account updates and mismatched private accounts after the sync.
    account_updates: AccountUpdates,
    /// The protocol configuration active at `block_num`. The node sends it when the sync starts at
    /// genesis, or when the starting block and `block_num` commit to different configurations.
    protocol_config: Option<ProtocolConfig>,
}

impl StateSyncUpdate {
    /// Assembles an update from its constituent parts, mirroring [`Self::into_parts`].
    pub fn from_parts(
        block_num: BlockNumber,
        partial_blockchain_updates: PartialBlockchainUpdates,
        note_updates: NoteUpdates,
        transaction_updates: TransactionUpdates,
        account_updates: AccountUpdates,
        protocol_config: Option<ProtocolConfig>,
    ) -> Self {
        Self {
            block_num,
            partial_blockchain_updates,
            note_updates,
            transaction_updates,
            account_updates,
            protocol_config,
        }
    }

    /// Returns the block number of the last synced block.
    pub fn block_num(&self) -> BlockNumber {
        self.block_num
    }

    /// Returns the partial blockchain updates.
    pub fn partial_blockchain_updates(&self) -> &PartialBlockchainUpdates {
        &self.partial_blockchain_updates
    }

    /// Returns the note updates.
    pub fn note_updates(&self) -> &NoteUpdates {
        &self.note_updates
    }

    /// Returns the transaction updates.
    pub fn transaction_updates(&self) -> &TransactionUpdates {
        &self.transaction_updates
    }

    /// Returns the account updates.
    pub fn account_updates(&self) -> &AccountUpdates {
        &self.account_updates
    }

    /// Returns the protocol configuration the node sent with this sync, if any.
    pub fn protocol_config(&self) -> Option<&ProtocolConfig> {
        self.protocol_config.as_ref()
    }

    /// Decomposes this update into its constituent parts.
    pub fn into_parts(
        self,
    ) -> (
        BlockNumber,
        PartialBlockchainUpdates,
        NoteUpdates,
        TransactionUpdates,
        AccountUpdates,
        Option<ProtocolConfig>,
    ) {
        (
            self.block_num,
            self.partial_blockchain_updates,
            self.note_updates,
            self.transaction_updates,
            self.account_updates,
            self.protocol_config,
        )
    }
}

/// Contains all the partial blockchain information that needs to be added in the client's store
/// after a sync: block headers, authentication nodes and the MMR peaks at the new sync height.
///
/// Insert-only: entries are staged once known to be worth keeping, never revised or removed.
#[derive(Debug, Clone, Default)]
pub struct PartialBlockchainUpdates {
    /// New block headers to be stored, keyed by block number. The value contains the block header
    /// and a flag indicating whether the block is relevant and should remain tracked.
    block_headers: BTreeMap<BlockNumber, (BlockHeader, bool)>,
    /// New authentication nodes that are meant to be stored in order to authenticate block headers.
    new_authentication_nodes: Vec<(InOrderIndex, Word)>,
    /// MMR peaks at the new sync height.
    pub new_peaks: MmrPeaks,
}

impl PartialBlockchainUpdates {
    /// Adds a block header to this [`PartialBlockchainUpdates`].
    ///
    /// On a repeated block number the `is_relevant` flag is OR-ed — the chain tip block may itself
    /// be relevant — so it only ever moves from `false` to `true`, matching
    /// [`Store::insert_block_header`](crate::store::Store::insert_block_header)'s one-way upgrade.
    pub fn insert(&mut self, block_header: BlockHeader, is_relevant: bool) {
        self.block_headers
            .entry(block_header.block_num())
            .and_modify(|(_, existing_is_relevant)| {
                *existing_is_relevant |= is_relevant;
            })
            .or_insert((block_header, is_relevant));
    }

    /// Stages authentication nodes for storage.
    ///
    /// Kept as one flat set rather than per-header, since tracked blocks' paths share internal
    /// nodes.
    pub fn extend_authentication_nodes(
        &mut self,
        nodes: impl IntoIterator<Item = (InOrderIndex, Word)>,
    ) {
        self.new_authentication_nodes.extend(nodes);
    }

    /// Returns the new block headers to be stored, along with a flag indicating whether each block
    /// is relevant and should remain tracked.
    pub fn block_headers(&self) -> impl Iterator<Item = &(BlockHeader, bool)> {
        self.block_headers.values()
    }

    /// Returns block headers that need to be persisted for this update.
    pub fn block_headers_to_store(
        &self,
        sync_height: BlockNumber,
    ) -> impl Iterator<Item = &(BlockHeader, bool)> {
        self.block_headers.values().filter(move |(header, is_relevant)| {
            *is_relevant
                || header.block_num() == BlockNumber::GENESIS
                || header.block_num() == sync_height
        })
    }

    /// Returns the new authentication nodes that are meant to be stored in order to authenticate
    /// block headers.
    pub fn new_authentication_nodes(&self) -> &[(InOrderIndex, Word)] {
        &self.new_authentication_nodes
    }
}

/// Final transaction records that the store must write for one sync.
#[derive(Debug, Default)]
pub struct TransactionUpdates {
    committed: Vec<TransactionRecord>,
    discarded: Vec<TransactionRecord>,
}

impl TransactionUpdates {
    /// Collects the terminal records. Pending records need no store write.
    pub fn new(records: impl IntoIterator<Item = TransactionRecord>) -> Self {
        let mut updates = Self::default();
        for record in records {
            match record.status {
                TransactionStatus::Committed { .. } => updates.committed.push(record),
                TransactionStatus::Discarded(_) => updates.discarded.push(record),
                TransactionStatus::Pending => {},
            }
        }
        updates
    }

    /// Returns the transactions committed in this sync.
    pub fn committed_transactions(&self) -> impl Iterator<Item = &TransactionRecord> {
        self.committed.iter()
    }

    /// Returns the transactions discarded in this sync.
    pub fn discarded_transactions(&self) -> impl Iterator<Item = &TransactionRecord> {
        self.discarded.iter()
    }

    /// Returns the IDs of all transactions changed by this sync.
    pub fn updated_transaction_ids(&self) -> impl Iterator<Item = TransactionId> + '_ {
        self.committed.iter().chain(&self.discarded).map(|tx| tx.id)
    }
}

// PUBLIC ACCOUNT UPDATE
// ================================================================================================

/// Update to a single tracked public account.
///
/// `StateSync` emits one of two variants depending on whether the node could return the account's
/// full state in a single response:
///
/// - [`PublicAccountUpdate::Full`] carries the new [`Account`] state directly (used when no storage
///   map is oversized and the vault fits in the response). The store applies it by replacing the
///   local state.
/// - [`PublicAccountUpdate::Patch`] carries the new account header plus the absolute
///   [`AccountPatch`] built from the node's incremental endpoints (`sync_storage_maps` and
///   `sync_account_vault`, used when any part of the account is oversized). The header is included
///   because the patch does not carry the final commitments.
#[derive(Debug, Clone)]
pub enum PublicAccountUpdate {
    /// The account fits in a single proof response — the new full state is carried as-is.
    Full(Account),
    /// The account is oversized in some dimension. The new state is described by the absolute
    /// patch, which advances the local state to `new_header`.
    Patch {
        /// The new account header after applying the patch.
        new_header: AccountHeader,
        /// The absolute patch to apply.
        patch: AccountPatch,
    },
}

impl PublicAccountUpdate {
    /// Returns the account ID for this update.
    pub fn id(&self) -> AccountId {
        match self {
            Self::Full(account) => account.id(),
            Self::Patch { new_header, .. } => new_header.id(),
        }
    }

    /// Returns the account nonce that this update advances the local state to.
    pub fn nonce(&self) -> Felt {
        match self {
            Self::Full(account) => account.nonce(),
            Self::Patch { new_header, .. } => new_header.nonce(),
        }
    }
}

// ACCOUNT UPDATES
// ================================================================================================

/// Contains account changes to apply to the store after a sync request.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_field_names)]
pub struct AccountUpdates {
    /// Updated public accounts, either as full state replacements or incremental patches.
    updated_public_accounts: Vec<PublicAccountUpdate>,
    /// Account commitments received from the network that don't match the currently locally-tracked
    /// state of the private accounts.
    ///
    /// These updates may represent a stale account commitment (meaning that the latest local state
    /// hasn't been committed). If this is not the case, the account may be locked until the state
    /// is restored manually.
    mismatched_private_accounts: Vec<(AccountId, Word)>,
    /// Witnesses validated at the target block, for the accounts the sync queried anyway. Kept so
    /// that the witness refresh does not request them a second time.
    account_witnesses: Vec<(AccountId, AccountWitness)>,
}

impl AccountUpdates {
    /// Creates a new instance of `AccountUpdates`.
    pub fn new(
        updated_public_accounts: Vec<PublicAccountUpdate>,
        mismatched_private_accounts: Vec<(AccountId, Word)>,
    ) -> Self {
        Self {
            updated_public_accounts,
            mismatched_private_accounts,
            account_witnesses: Vec::new(),
        }
    }

    /// Attaches the account witnesses the sync validated at its target block.
    #[must_use]
    pub fn with_account_witnesses(
        mut self,
        account_witnesses: Vec<(AccountId, AccountWitness)>,
    ) -> Self {
        self.account_witnesses = account_witnesses;
        self
    }

    /// Returns the updated public accounts.
    pub fn updated_public_accounts(&self) -> &[PublicAccountUpdate] {
        &self.updated_public_accounts
    }

    /// Returns the mismatched private accounts.
    pub fn mismatched_private_accounts(&self) -> &[(AccountId, Word)] {
        &self.mismatched_private_accounts
    }

    /// Returns the account witnesses validated at the sync's target block.
    pub fn account_witnesses(&self) -> &[(AccountId, AccountWitness)] {
        &self.account_witnesses
    }
}

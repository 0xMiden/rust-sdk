use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use miden_protocol::account::{Account, AccountHeader, AccountId, AccountPatch};
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, MmrPeaks};
use miden_protocol::note::Nullifier;
use miden_protocol::protocol_config::ProtocolConfig;
use miden_protocol::transaction::{TransactionHeader, TransactionId};
use miden_protocol::{Felt, Word};

use crate::store::NoteUpdateTracker;
use crate::transaction::{DiscardCause, TransactionRecord, TransactionStatus};

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
    note_updates: NoteUpdateTracker,
    /// Committed and discarded transactions after the sync.
    transaction_updates: TransactionUpdateTracker,
    /// Public account updates and mismatched private accounts after the sync.
    account_updates: AccountUpdates,
    /// The protocol configuration active at `block_num`. The node sends it when the sync starts at
    /// genesis, or when the starting block and `block_num` commit to different configurations.
    protocol_config: Option<ProtocolConfig>,
}

impl StateSyncUpdate {
    /// Assembles an update from its constituent parts, mirroring [`Self::into_parts`].
    ///
    /// The parts are stored as given: no validation or minimization is applied.
    pub fn from_parts(
        block_num: BlockNumber,
        partial_blockchain_updates: PartialBlockchainUpdates,
        note_updates: NoteUpdateTracker,
        transaction_updates: TransactionUpdateTracker,
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
    pub fn note_updates(&self) -> &NoteUpdateTracker {
        &self.note_updates
    }

    /// Returns the transaction updates.
    pub fn transaction_updates(&self) -> &TransactionUpdateTracker {
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
        NoteUpdateTracker,
        TransactionUpdateTracker,
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

/// Contains transaction changes to apply to the store.
#[derive(Default)]
pub struct TransactionUpdateTracker {
    /// Transactions that were committed in the block.
    transactions: BTreeMap<TransactionId, TransactionRecord>,
    /// Nullifier-to-account mappings from external transactions by tracked accounts.
    external_nullifier_accounts: BTreeMap<Nullifier, AccountId>,
}

impl TransactionUpdateTracker {
    /// Creates a new [`TransactionUpdateTracker`]
    pub fn new(transactions: Vec<TransactionRecord>) -> Self {
        let transactions =
            transactions.into_iter().map(|tx| (tx.id, tx)).collect::<BTreeMap<_, _>>();

        Self {
            transactions,
            external_nullifier_accounts: BTreeMap::new(),
        }
    }

    /// Returns a reference to committed transactions.
    pub fn committed_transactions(&self) -> impl Iterator<Item = &TransactionRecord> {
        self.transactions
            .values()
            .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
    }

    /// Returns a reference to discarded transactions.
    pub fn discarded_transactions(&self) -> impl Iterator<Item = &TransactionRecord> {
        self.transactions
            .values()
            .filter(|tx| matches!(tx.status, TransactionStatus::Discarded(_)))
    }

    /// Returns a mutable reference to pending transactions in the tracker.
    fn mutable_pending_transactions(&mut self) -> impl Iterator<Item = &mut TransactionRecord> {
        self.transactions
            .values_mut()
            .filter(|tx| matches!(tx.status, TransactionStatus::Pending))
    }

    /// Returns transaction IDs of all transactions that have been updated.
    pub fn updated_transaction_ids(&self) -> impl Iterator<Item = TransactionId> {
        self.committed_transactions()
            .chain(self.discarded_transactions())
            .map(|tx| tx.id)
    }

    /// Returns the account ID that consumed the given nullifier in an external transaction, if
    /// available.
    pub fn external_nullifier_account(&self, nullifier: &Nullifier) -> Option<AccountId> {
        self.external_nullifier_accounts.get(nullifier).copied()
    }

    /// Applies the necessary state transitions to the [`TransactionUpdateTracker`] when a
    /// transaction is included in a block.
    ///
    /// The included transaction is matched to a local pending transaction by its ID only. The node
    /// reports the original transaction ID, so a record with an unknown ID is an external
    /// transaction of a tracked account.
    pub fn apply_transaction_inclusion(
        &mut self,
        block_num: BlockNumber,
        header: &TransactionHeader,
        timestamp: u64,
    ) {
        let account_id = header.account_id();

        if let Some(transaction) = self.transactions.get_mut(&header.id()) {
            transaction.commit_transaction(block_num, timestamp);
            return;
        }

        // No local transaction has this ID. This is an external transaction by a tracked account.
        // Record the nullifier→account mappings so we can attribute note consumption to tracked
        // accounts during nullifier processing.
        for commitment in header.input_notes().iter() {
            self.external_nullifier_accounts.insert(commitment.nullifier(), account_id);
        }
    }

    /// Applies the necessary state transitions to the [`TransactionUpdateTracker`] when a the sync
    /// height of the client is updated. This may result in stale or expired transactions.
    pub fn apply_sync_height_update(
        &mut self,
        new_sync_height: BlockNumber,
        tx_discard_delta: Option<u32>,
    ) {
        if let Some(tx_discard_delta) = tx_discard_delta {
            self.discard_transaction_with_predicate(
                |transaction| {
                    transaction.details.submission_height
                        < new_sync_height.checked_sub(tx_discard_delta).unwrap_or_default()
                },
                DiscardCause::Stale,
            );
        }

        // NOTE: we check for <= new_sync height because at this point we would have committed the
        // transaction otherwise
        self.discard_transaction_with_predicate(
            |transaction| transaction.details.expiration_block_num <= new_sync_height,
            DiscardCause::Expired,
        );
    }

    /// Applies the necessary state transitions to the [`TransactionUpdateTracker`] when a note is
    /// nullified. this may result in transactions being discarded because they were processing the
    /// nullified note.
    pub fn apply_input_note_nullified(&mut self, input_note_nullifier: Nullifier) {
        self.discard_transaction_with_predicate(
            |transaction| {
                // Check if the note was being processed by a local transaction that didn't end up
                // being committed so it should be discarded
                transaction
                    .details
                    .input_note_nullifiers
                    .contains(&input_note_nullifier.as_word())
            },
            DiscardCause::InputConsumed,
        );
    }

    /// Discards the local transaction that produced this now-superseded account state.
    pub fn apply_superseded_account_state(&mut self, superseded_account_state: Word) {
        self.discard_transaction_with_predicate(
            |transaction| transaction.details.final_account_state == superseded_account_state,
            DiscardCause::Superseded,
        );
    }

    /// Discards transactions that have the same initial account state as the provided one.
    pub fn apply_invalid_initial_account_state(&mut self, invalid_account_state: Word) {
        self.discard_transaction_with_predicate(
            |transaction| transaction.details.init_account_state == invalid_account_state,
            DiscardCause::DiscardedInitialState,
        );
    }

    /// Discards transactions that match the predicate and also applies the new invalid account
    /// states
    fn discard_transaction_with_predicate<F>(&mut self, predicate: F, discard_cause: DiscardCause)
    where
        F: Fn(&TransactionRecord) -> bool,
    {
        let mut new_invalid_account_states = vec![];

        for transaction in self.mutable_pending_transactions() {
            // Discard transactions, and also push the invalid account state if the transaction got
            // correctly discarded
            // NOTE: previous updates in a chain of state syncs could have committed a transaction,
            // so we need to check that `discard_transaction` returns `true` here (aka, it got
            // discarded from a valid state)
            if predicate(transaction) && transaction.discard_transaction(discard_cause) {
                new_invalid_account_states.push(transaction.details.final_account_state);
            }
        }

        for state in new_invalid_account_states {
            self.apply_invalid_initial_account_state(state);
        }
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

    pub fn extend(&mut self, other: AccountUpdates) {
        self.updated_public_accounts.extend(other.updated_public_accounts);
        self.mismatched_private_accounts.extend(other.mismatched_private_accounts);
        self.account_witnesses.extend(other.account_witnesses);
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;
    use miden_protocol::transaction::{InputNoteCommitment, InputNotes, RawOutputNotes};

    use super::*;
    use crate::transaction::TransactionDetails;

    fn account_id() -> AccountId {
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap()
    }

    fn word(n: u64) -> Word {
        Word::from([
            Felt::new_unchecked(n),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
        ])
    }

    // TRANSACTION INCLUSION TESTS
    // --------------------------------------------------------------------------------------------

    fn transaction_header(init_state: u64, final_state: u64, nullifier: u64) -> TransactionHeader {
        let input_notes = InputNotes::new_unchecked(vec![InputNoteCommitment::from(
            Nullifier::from_raw(word(nullifier)),
        )]);

        TransactionHeader::new(
            account_id(),
            word(init_state),
            word(final_state),
            input_notes,
            vec![],
        )
        .unwrap()
    }

    fn pending_transaction(init_state: u64, final_state: u64, nullifier: u64) -> TransactionRecord {
        let id = transaction_header(init_state, final_state, nullifier).id();
        let details = TransactionDetails {
            account_id: account_id(),
            init_account_state: word(init_state),
            final_account_state: word(final_state),
            input_note_nullifiers: vec![word(nullifier)],
            output_notes: RawOutputNotes::new(vec![]).unwrap(),
            block_num: BlockNumber::from(1u32),
            submission_height: BlockNumber::from(1u32),
            expiration_block_num: BlockNumber::from(100u32),
            creation_timestamp: 0,
        };

        TransactionRecord::new(id, details, None, TransactionStatus::Pending)
    }

    /// An included transaction with an unknown ID is external, even when it shares the account and
    /// the initial and final states with a local pending transaction.
    #[test]
    fn inclusion_with_unknown_id_does_not_commit_local_transaction() {
        let local = pending_transaction(10, 11, 1);
        let local_id = local.id;
        let mut tracker = TransactionUpdateTracker::new(vec![local]);

        let included = transaction_header(10, 11, 2);
        assert_ne!(included.id(), local_id);
        tracker.apply_transaction_inclusion(BlockNumber::from(5u32), &included, 0);

        assert!(tracker.committed_transactions().next().is_none());
        assert_eq!(
            tracker.external_nullifier_account(&Nullifier::from_raw(word(2))),
            Some(account_id())
        );
    }
}

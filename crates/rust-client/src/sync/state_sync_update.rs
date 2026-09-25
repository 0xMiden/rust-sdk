use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use miden_protocol::account::{
    AccountHeader,
    AccountId,
    AccountStoragePatch,
    StorageMapPatch,
    StorageMapPatchEntries,
    StorageSlotName,
    StorageSlotPatch,
    StorageValuePatch,
};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, MmrPeaks};
use miden_protocol::errors::AccountPatchError;
use miden_protocol::note::{NoteId, Nullifier};
use miden_protocol::protocol_config::ProtocolConfig;
use miden_protocol::transaction::TransactionId;
use miden_protocol::{ONE, Word};

use super::SyncSummary;
use crate::note::{NoteUpdateTracker, NoteUpdateType};
use crate::rpc::domain::transaction::TransactionRecord as RpcTransactionRecord;
use crate::store::{AccountStateUpdate, AccountUpdates};
use crate::transaction::{DiscardCause, TransactionRecord, TransactionStatus};

// STATE SYNC UPDATE
// ================================================================================================

/// Contains all information needed to apply the update in the store after syncing with the node.
///
/// Immutable once built: [`StateSync::sync_state`](super::StateSync::sync_state) assembles the
/// individual trackers and seals them into this type at the end of the sync pass. Use
/// [`Self::from_parts`] to build one directly.
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

impl From<&StateSyncUpdate> for SyncSummary {
    fn from(value: &StateSyncUpdate) -> Self {
        let new_public_note_ids = value
            .note_updates
            .updated_input_notes()
            .filter_map(|note_update| {
                let note = note_update.inner();
                if let NoteUpdateType::Insert = note_update.update_type() {
                    note.id()
                } else {
                    None
                }
            })
            .collect();

        let committed_note_ids: BTreeSet<NoteId> = value
            .note_updates
            .updated_input_notes()
            .filter_map(|note_update| {
                let note = note_update.inner();
                // `InsertCommitted` is a previously-tracked expected note that just committed, so
                // it counts as committed (not as a newly-discovered note) even though it is
                // persisted via a full-row insert.
                if matches!(
                    note_update.update_type(),
                    NoteUpdateType::Update | NoteUpdateType::InsertCommitted
                ) && note.is_committed()
                {
                    note.id()
                } else {
                    None
                }
            })
            .chain(value.note_updates.updated_output_notes().filter_map(|note_update| {
                let note = note_update.inner();
                if let NoteUpdateType::Update = note_update.update_type() {
                    note.is_committed().then_some(note.id())
                } else {
                    None
                }
            }))
            .collect();

        let consumed_note_ids: BTreeSet<NoteId> =
            value.note_updates.consumed_input_note_ids().collect();

        SyncSummary::new(
            value.block_num,
            new_public_note_ids,
            // Populated by Client::sync_state from the Note Transport Layer fetch.
            Vec::new(),
            committed_note_ids.into_iter().collect(),
            consumed_note_ids.into_iter().collect(),
            value
                .account_updates
                .updated_public_accounts()
                .iter()
                .map(AccountStateUpdate::id)
                .collect(),
            value
                .account_updates
                .mismatched_private_accounts()
                .iter()
                .map(|(id, _)| *id)
                .collect(),
            value.transaction_updates.committed_transactions().map(|t| t.id).collect(),
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
    pub fn apply_transaction_inclusion(&mut self, record: &RpcTransactionRecord, timestamp: u64) {
        let header = &record.transaction_header;
        let account_id = header.account_id();

        if let Some(transaction) = self.transactions.get_mut(&header.id()) {
            transaction.commit_transaction(record.block_num, timestamp);
            return;
        }

        // Fallback for transactions with unauthenticated input notes: the node authenticates these
        // notes during processing, which changes the transaction ID. Match by account ID and
        // pre-transaction state instead.
        if let Some(transaction) = self.transactions.values_mut().find(|tx| {
            tx.details.account_id == account_id
                && tx.details.init_account_state == header.initial_state_commitment()
        }) {
            transaction.commit_transaction(record.block_num, timestamp);
            return;
        }

        // No local transaction matched. This is an external transaction by a tracked account.
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

// STORAGE PATCH BUILDER
// ================================================================================================

/// Builds the absolute [`AccountStoragePatch`] implied by the updates fetched from the node: the
/// value-slot values, the absolute changed map entries per slot, and the complete entries of the
/// maps the node returned in full.
///
/// The carried updates are already merged to the new absolute value of each changed storage slot
/// and map entry, so the patch is assembled directly from them with no need to load the prior
/// account state.
///
/// `changed_map_entries` become `Update` patches layered onto the local map. `complete_map_entries`
/// become `Create` patches, which the store applies as a full replacement of the slot, so they need
/// no removal information.
///
/// A newly created account (final nonce 1) has no prior slots to update, so every value slot and
/// changed map is emitted as a `Create`.
pub(crate) fn build_storage_patch(
    new_header: &AccountHeader,
    value_slot_updates: Vec<(StorageSlotName, Word)>,
    changed_map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
    complete_map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
) -> Result<AccountStoragePatch, AccountPatchError> {
    let is_full_state = new_header.nonce() == ONE;

    let value_entries = value_slot_updates.into_iter().map(|(slot_name, new_value)| {
        let value_patch = if is_full_state {
            StorageValuePatch::Create { value: new_value }
        } else {
            StorageValuePatch::Update { value: new_value }
        };
        (slot_name, StorageSlotPatch::Value(value_patch))
    });

    let changed_maps = changed_map_entries.into_iter().map(|(slot_name, entries)| {
        let map_patch = if is_full_state {
            StorageMapPatch::Create { entries }
        } else {
            StorageMapPatch::Update { entries }
        };
        (slot_name, StorageSlotPatch::Map(map_patch))
    });

    let complete_maps = complete_map_entries.into_iter().map(|(slot_name, entries)| {
        (slot_name, StorageSlotPatch::Map(StorageMapPatch::Create { entries }))
    });

    AccountStoragePatch::from_entries(value_entries.chain(changed_maps).chain(complete_maps))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::vec;

    use miden_protocol::Felt;
    use miden_protocol::account::{StorageMapKey, StorageMapPatchEntries};
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;

    use super::*;

    fn account_id() -> AccountId {
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap()
    }

    fn slot_name(name: &str) -> StorageSlotName {
        StorageSlotName::new(name).unwrap()
    }

    fn word(n: u64) -> Word {
        Word::from([
            Felt::new_unchecked(n),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
        ])
    }

    fn header_with_nonce(nonce: u64) -> AccountHeader {
        AccountHeader::new(
            account_id(),
            Felt::new(nonce).expect("test nonce must be a valid Felt"),
            Word::default(),
            Word::default(),
            Word::default(),
        )
    }

    fn build_patch(
        new_nonce: u64,
        value_slot_updates: Vec<(StorageSlotName, Word)>,
        map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
    ) -> Result<AccountStoragePatch, AccountPatchError> {
        build_storage_patch(
            &header_with_nonce(new_nonce),
            value_slot_updates,
            map_entries,
            BTreeMap::new(),
        )
    }

    /// A map the node returned in full becomes a `Create` patch even for an existing account, so
    /// the store replaces the slot instead of layering the entries onto it.
    #[test]
    fn build_patch_creates_complete_maps_for_existing_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let complete_map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_storage_patch(
            &header_with_nonce(2),
            vec![],
            BTreeMap::new(),
            complete_map_entries,
        )
        .unwrap();

        let (_, map_patch) = patch.maps().next().expect("patch should contain map slot");
        assert!(matches!(map_patch, StorageMapPatch::Create { .. }));
    }

    #[test]
    fn build_patch_empty_payload_is_empty() {
        let patch = build_patch(4, vec![], BTreeMap::new()).unwrap();

        assert!(patch.is_empty());
    }

    #[test]
    fn build_patch_sets_value_slot_absolutely() {
        let value_slot = slot_name("miden::test::value");
        let patch = build_patch(2, vec![(value_slot.clone(), word(2))], BTreeMap::new()).unwrap();

        assert_eq!(patch.updated_value(&value_slot), Some(word(2)));
    }

    #[test]
    fn build_patch_wraps_merged_map_entries() {
        let map_slot = slot_name("miden::test::map");
        let key = StorageMapKey::from_raw(word(42));
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(key, word(300));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_patch(2, vec![], map_entries).unwrap();

        let entries = patch.updated_map(&map_slot).expect("patch should contain map slot");
        assert_eq!(entries.as_map().len(), 1);
        assert_eq!(*entries.as_map().values().next().unwrap(), word(300));
    }

    /// A newly created account (final nonce 1) has no prior slots, so its value slots are emitted
    /// as `Create` rather than `Update`.
    #[test]
    fn build_patch_for_new_account_creates_value_slots() {
        let value_slot = slot_name("miden::test::value");
        let patch = build_patch(1, vec![(value_slot, word(1))], BTreeMap::new()).unwrap();

        let (_, value_patch) = patch.values().next().expect("patch should contain value slot");
        assert!(matches!(value_patch, StorageValuePatch::Create { .. }));
    }

    /// A newly created account (final nonce 1, full-state) emits each map slot as a `Create`, which
    /// the store applies by starting the slot from an empty map.
    #[test]
    fn build_patch_emits_map_create_for_new_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_patch(1, vec![], map_entries).unwrap();

        assert!(patch.created_map(&map_slot).is_some());
    }

    /// An update to an existing account (final nonce > 1) emits map slots as `Update`, never
    /// `Create`, so the sync path never asks the store to re-create a populated map.
    #[test]
    fn build_patch_emits_map_update_for_existing_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_patch(2, vec![], map_entries).unwrap();

        assert!(patch.updated_map(&map_slot).is_some());
    }
}

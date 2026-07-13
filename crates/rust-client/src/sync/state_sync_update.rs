use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use miden_protocol::account::{
    Account,
    AccountCode,
    AccountHeader,
    AccountId,
    AccountPatch,
    AccountStoragePatch,
    AccountVaultPatch,
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
use miden_protocol::transaction::TransactionId;
use miden_protocol::{Felt, ONE, Word};

use super::SyncSummary;
use crate::note::{NoteUpdateTracker, NoteUpdateType};
use crate::rpc::domain::transaction::TransactionRecord as RpcTransactionRecord;
use crate::transaction::{DiscardCause, TransactionRecord, TransactionStatus};

// STATE SYNC UPDATE
// ================================================================================================

/// Contains all information needed to apply the update in the store after syncing with the node.
#[derive(Default)]
pub struct StateSyncUpdate {
    /// The block number of the last block that was synced.
    pub block_num: BlockNumber,
    /// New blocks, authentication nodes and MMR peaks.
    pub partial_blockchain_updates: PartialBlockchainUpdates,
    /// New and updated notes to be upserted in the store.
    pub note_updates: NoteUpdateTracker,
    /// Committed and discarded transactions after the sync.
    pub transaction_updates: TransactionUpdateTracker,
    /// Public account updates and mismatched private accounts after the sync.
    pub account_updates: AccountUpdates,
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
                .map(PublicAccountUpdate::id)
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
#[derive(Debug, Clone, Default)]
pub struct PartialBlockchainUpdates {
    /// New block headers to be stored, keyed by block number. The value contains the block
    /// header and a flag indicating whether the block contains notes relevant to the client.
    block_headers: BTreeMap<BlockNumber, (BlockHeader, bool)>,
    /// New authentication nodes that are meant to be stored in order to authenticate block
    /// headers.
    new_authentication_nodes: Vec<(InOrderIndex, Word)>,
    /// MMR peaks at the new sync height.
    pub new_peaks: MmrPeaks,
}

impl PartialBlockchainUpdates {
    /// Adds or updates a block header in this [`PartialBlockchainUpdates`].
    ///
    /// If the block header already exists (same block number), the `has_client_notes` flag is
    /// OR-ed. Otherwise a new entry is added.
    pub fn insert(
        &mut self,
        block_header: BlockHeader,
        has_client_notes: bool,
        new_authentication_nodes: Vec<(InOrderIndex, Word)>,
    ) {
        self.block_headers
            .entry(block_header.block_num())
            .and_modify(|(_, existing_has_notes)| {
                *existing_has_notes |= has_client_notes;
            })
            .or_insert((block_header, has_client_notes));

        self.new_authentication_nodes.extend(new_authentication_nodes);
    }

    /// Returns the new block headers to be stored, along with a flag indicating whether the block
    /// contains notes that are relevant to the client.
    pub fn block_headers(&self) -> impl Iterator<Item = &(BlockHeader, bool)> {
        self.block_headers.values()
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

        // Fallback for transactions with unauthenticated input notes: the node
        // authenticates these notes during processing, which changes the transaction
        // ID. Match by account ID and pre-transaction state instead.
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
            // Discard transactions, and also push the invalid account state if the transaction
            // got correctly discarded
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
/// - [`PublicAccountUpdate::Patch`] carries a [`PublicAccountPatch`] payload (new header plus
///   incremental updates from `sync_storage_maps` and `sync_account_vault`, used when any part of
///   the account is oversized). The store calls [`PublicAccountPatch::compute_account_patch`] to
///   build the absolute [`AccountPatch`] to apply.
#[derive(Debug, Clone)]
pub enum PublicAccountUpdate {
    /// The account fits in a single proof response — the new full state is carried as-is.
    Full(Account),
    /// The account is oversized in some dimension. The new state is described by the absolute
    /// patch built from the carried incremental updates.
    Patch(PublicAccountPatch),
}

impl PublicAccountUpdate {
    /// Returns the account ID for this update.
    pub fn id(&self) -> AccountId {
        match self {
            Self::Full(account) => account.id(),
            Self::Patch(patch) => patch.id(),
        }
    }

    /// Returns the account nonce that this update advances the local state to.
    pub fn nonce(&self) -> Felt {
        match self {
            Self::Full(account) => account.nonce(),
            Self::Patch(patch) => patch.new_header().nonce(),
        }
    }
}

/// Patch payload for a public account update.
///
/// Carries the new account header plus the merged updates fetched from the node's incremental
/// endpoints (`sync_storage_maps` and `sync_account_vault`): the value-slot values, the absolute
/// changed map entries per slot, and the absolute vault patch. The store turns these into the
/// [`AccountPatch`] to apply via [`Self::compute_account_patch`].
#[derive(Debug, Clone)]
pub struct PublicAccountPatch {
    /// The new account header after applying these updates.
    new_header: AccountHeader,
    /// New value-slot values from the `get_account` storage header. Value slots are always
    /// small enough to fit in the response.
    value_slot_updates: Vec<(StorageSlotName, Word)>,
    /// Absolute changed entries per storage map slot, merged from `sync_storage_maps`.
    map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
    /// Absolute vault patch merged from `sync_account_vault`.
    vault_patch: AccountVaultPatch,
}

impl PublicAccountPatch {
    /// Creates a new [`PublicAccountPatch`].
    pub fn new(
        new_header: AccountHeader,
        value_slot_updates: Vec<(StorageSlotName, Word)>,
        map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
        vault_patch: AccountVaultPatch,
    ) -> Self {
        Self {
            new_header,
            value_slot_updates,
            map_entries,
            vault_patch,
        }
    }

    /// Returns the account ID this patch applies to.
    pub fn id(&self) -> AccountId {
        self.new_header.id()
    }

    /// Returns the new account header that this patch advances the local state to.
    pub fn new_header(&self) -> &AccountHeader {
        &self.new_header
    }

    /// Builds the absolute [`AccountPatch`] implied by this payload.
    ///
    /// The carried updates are already merged to the new absolute value of each changed storage
    /// slot, map entry, and vault asset, so the patch is assembled directly from them with no need
    /// to load the prior account state.
    ///
    /// An update of an existing account (final nonce > 1) yields a partial-state patch with no
    /// code. A newly created account (final nonce 1) cannot be represented as a partial-state
    /// patch, so it must carry the account code: the caller supplies `code` (the account's code
    /// is fixed at creation, so the locally-tracked code matches), and the patch becomes a
    /// full-state patch.
    pub fn compute_account_patch(
        &self,
        code: Option<AccountCode>,
    ) -> Result<AccountPatch, AccountPatchError> {
        let account_id = self.new_header.id();

        let is_full_state = self.new_header.nonce() == ONE;

        let value_entries = self.value_slot_updates.iter().map(|(slot_name, new_value)| {
            let value_patch = if is_full_state {
                StorageValuePatch::Create { value: *new_value }
            } else {
                StorageValuePatch::Update { value: *new_value }
            };
            (slot_name.clone(), StorageSlotPatch::Value(value_patch))
        });

        let map_entries = self.map_entries.iter().map(|(slot_name, entries)| {
            let map_patch = if is_full_state {
                StorageMapPatch::Create { entries: entries.clone() }
            } else {
                StorageMapPatch::Update { entries: entries.clone() }
            };
            (slot_name.clone(), StorageSlotPatch::Map(map_patch))
        });

        let storage = AccountStoragePatch::from_entries(value_entries.chain(map_entries))?;

        let code = if is_full_state { code } else { None };

        AccountPatch::new(
            account_id,
            storage,
            self.vault_patch.clone(),
            code,
            Some(self.new_header.nonce()),
        )
    }
}

// ACCOUNT UPDATES
// ================================================================================================

/// Contains account changes to apply to the store after a sync request.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_field_names)]
pub struct AccountUpdates {
    /// Updated public accounts, either as full state replacements or incremental deltas.
    updated_public_accounts: Vec<PublicAccountUpdate>,
    /// Account commitments received from the network that don't match the currently
    /// locally-tracked state of the private accounts.
    ///
    /// These updates may represent a stale account commitment (meaning that the latest local state
    /// hasn't been committed). If this is not the case, the account may be locked until the state
    /// is restored manually.
    mismatched_private_accounts: Vec<(AccountId, Word)>,
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
        }
    }

    /// Returns the updated public accounts.
    pub fn updated_public_accounts(&self) -> &[PublicAccountUpdate] {
        &self.updated_public_accounts
    }

    /// Returns the mismatched private accounts.
    pub fn mismatched_private_accounts(&self) -> &[(AccountId, Word)] {
        &self.mismatched_private_accounts
    }

    pub fn extend(&mut self, other: AccountUpdates) {
        self.updated_public_accounts.extend(other.updated_public_accounts);
        self.mismatched_private_accounts.extend(other.mismatched_private_accounts);
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::vec;

    use miden_protocol::account::{AccountCode, StorageMapKey, StorageMapPatchEntries};
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

    fn payload(
        new_nonce: u64,
        value_slot_updates: Vec<(StorageSlotName, Word)>,
        map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
    ) -> PublicAccountPatch {
        PublicAccountPatch::new(
            header_with_nonce(new_nonce),
            value_slot_updates,
            map_entries,
            AccountVaultPatch::default(),
        )
    }

    fn empty_payload(new_header: AccountHeader) -> PublicAccountPatch {
        PublicAccountPatch::new(new_header, vec![], BTreeMap::new(), AccountVaultPatch::default())
    }

    #[test]
    fn compute_patch_empty_payload_carries_only_nonce() {
        let patch = empty_payload(header_with_nonce(4)).compute_account_patch(None).unwrap();

        assert_eq!(patch.final_nonce(), Some(Felt::new_unchecked(4)));
        assert!(patch.storage().is_empty());
        assert!(patch.vault().is_empty());
        assert!(!patch.is_full_state());
    }

    #[test]
    fn compute_patch_sets_value_slot_absolutely() {
        let value_slot = slot_name("miden::test::value");
        let patch = payload(2, vec![(value_slot.clone(), word(2))], BTreeMap::new())
            .compute_account_patch(None)
            .unwrap();

        assert_eq!(patch.storage().get_value(&value_slot), Some(word(2)));
    }

    #[test]
    fn compute_patch_wraps_merged_map_entries() {
        let map_slot = slot_name("miden::test::map");
        let key = StorageMapKey::from_raw(word(42));
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(key, word(300));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = payload(2, vec![], map_entries).compute_account_patch(None).unwrap();

        let map = patch.storage().get_map(&map_slot).expect("patch should contain map slot");
        assert_eq!(map.entries().unwrap().as_map().len(), 1);
        assert_eq!(*map.entries().unwrap().as_map().values().next().unwrap(), word(300));
    }

    #[test]
    fn compute_patch_rejects_zero_nonce() {
        let result = empty_payload(header_with_nonce(0)).compute_account_patch(None);
        assert!(result.is_err());
    }

    /// A newly created account (final nonce 1) observed via the oversized sync path yields a
    /// full-state patch carrying the supplied code, rather than failing to build.
    #[test]
    fn compute_patch_for_new_account_is_full_state() {
        let value_slot = slot_name("miden::test::value");
        let patch = payload(1, vec![(value_slot, word(1))], BTreeMap::new())
            .compute_account_patch(Some(AccountCode::mock()))
            .unwrap();

        assert!(patch.is_full_state());
        assert_eq!(patch.final_nonce(), Some(ONE));
    }

    /// A final-nonce-1 patch cannot be built without the account code (it must be a full-state
    /// patch), so the missing-code case is reported rather than silently producing a wrong patch.
    #[test]
    fn compute_patch_for_new_account_without_code_errors() {
        let result = payload(1, vec![], BTreeMap::new()).compute_account_patch(None);
        assert!(result.is_err());
    }

    /// A newly created account (final nonce 1, full-state) emits each map slot as a `Create`, which
    /// the store applies by starting the slot from an empty map.
    #[test]
    fn compute_patch_emits_map_create_for_new_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = payload(1, vec![], map_entries)
            .compute_account_patch(Some(AccountCode::mock()))
            .unwrap();

        let map = patch.storage().get_map(&map_slot).expect("patch should contain map slot");
        assert!(matches!(map, StorageMapPatch::Create { .. }));
    }

    /// An update to an existing account (final nonce > 1) emits map slots as `Update`, never
    /// `Create`, so the sync path never asks the store to re-create a populated map.
    #[test]
    fn compute_patch_emits_map_update_for_existing_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = payload(2, vec![], map_entries).compute_account_patch(None).unwrap();

        let map = patch.storage().get_map(&map_slot).expect("patch should contain map slot");
        assert!(matches!(map, StorageMapPatch::Update { .. }));
    }
}

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use miden_protocol::account::{
    Account,
    AccountDelta,
    AccountHeader,
    AccountId,
    AccountStorage,
    AccountStorageDelta,
    AccountVaultDelta,
    StorageMapKey,
    StorageSlotName,
};
use miden_protocol::asset::{Asset, AssetVault, AssetVaultKey};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, MmrPeaks};
use miden_protocol::errors::{AccountDeltaError, AccountError};
use miden_protocol::note::{NoteId, Nullifier};
use miden_protocol::transaction::TransactionId;
use miden_protocol::{Felt, Word};

use super::SyncSummary;
use crate::note::{NoteUpdateTracker, NoteUpdateType};
use crate::rpc::domain::account_vault::AccountVaultUpdate;
use crate::rpc::domain::storage_map::StorageMapUpdate;
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
///   local state — no delta computation needed.
/// - [`PublicAccountUpdate::Delta`] carries a [`PublicAccountDelta`] payload (new header plus
///   incremental updates from `sync_storage_maps` and `sync_account_vault`, used when any part of
///   the account is oversized). The store calls [`PublicAccountDelta::compute_account_delta`] to
///   derive the [`AccountDelta`] to apply.
#[derive(Debug, Clone)]
pub enum PublicAccountUpdate {
    /// The account fits in a single proof response — the new full state is carried as-is.
    Full(Account),
    /// The account is oversized in some dimension. The new state must be reconstructed by
    /// replaying the carried incremental updates against the locally-stored state.
    Delta(PublicAccountDelta),
}

impl PublicAccountUpdate {
    /// Returns the account ID for this update.
    pub fn id(&self) -> AccountId {
        match self {
            Self::Full(account) => account.id(),
            Self::Delta(delta) => delta.id(),
        }
    }

    /// Returns the account nonce that this update advances the local state to.
    pub fn nonce(&self) -> Felt {
        match self {
            Self::Full(account) => account.nonce(),
            Self::Delta(delta) => delta.new_header().nonce(),
        }
    }
}

/// Incremental delta payload for a public account update.
///
/// Carries the new account header plus the per-block updates fetched from the node's incremental
/// endpoints (`sync_storage_maps` and `sync_account_vault`). The store derives the
/// [`AccountDelta`] to apply by replaying these updates against its locally-stored account state
/// via [`Self::compute_account_delta`].
#[derive(Debug, Clone)]
pub struct PublicAccountDelta {
    /// The new account header after applying these updates.
    new_header: AccountHeader,
    /// First block of the synced range (the client's previous sync height).
    block_from: BlockNumber,
    /// Last block of the synced range (the block at which `new_header` is observed).
    block_to: BlockNumber,
    /// New value-slot values from the `get_account` storage header. Value slots are always
    /// small enough to fit in the response.
    value_slot_updates: Vec<(StorageSlotName, Word)>,
    /// Per-block storage map updates from `sync_storage_maps`.
    storage_map_updates: Vec<StorageMapUpdate>,
    /// Per-block vault updates from `sync_account_vault`.
    vault_updates: Vec<AccountVaultUpdate>,
}

impl PublicAccountDelta {
    /// Creates a new [`PublicAccountDelta`].
    pub fn new(
        new_header: AccountHeader,
        block_from: BlockNumber,
        block_to: BlockNumber,
        value_slot_updates: Vec<(StorageSlotName, Word)>,
        storage_map_updates: Vec<StorageMapUpdate>,
        vault_updates: Vec<AccountVaultUpdate>,
    ) -> Self {
        Self {
            new_header,
            block_from,
            block_to,
            value_slot_updates,
            storage_map_updates,
            vault_updates,
        }
    }

    /// Returns the account ID this delta applies to.
    pub fn id(&self) -> AccountId {
        self.new_header.id()
    }

    /// Returns the new account header that this delta advances the local state to.
    pub fn new_header(&self) -> &AccountHeader {
        &self.new_header
    }

    /// Returns the first block of the synced range.
    pub fn block_from(&self) -> BlockNumber {
        self.block_from
    }

    /// Returns the names of the value slots referenced by this delta. The store can use this to
    /// load only the slots needed by [`Self::compute_account_delta`] instead of the full storage.
    pub fn value_slot_names(&self) -> Vec<StorageSlotName> {
        self.value_slot_updates.iter().map(|(name, _)| name.clone()).collect()
    }

    /// Returns the last block of the synced range.
    pub fn block_to(&self) -> BlockNumber {
        self.block_to
    }

    /// Computes the [`AccountDelta`] implied by this payload by replaying the carried
    /// incremental updates against the locally-stored account state.
    // TODO #2171:
    // skip building AccountDelta; have the store accept raw RPC updates directly.
    pub fn compute_account_delta(
        &self,
        local_header: &AccountHeader,
        local_storage: &AccountStorage,
        local_vault: &AssetVault,
    ) -> Result<AccountDelta, AccountDeltaError> {
        let old_nonce = local_header.nonce().as_canonical_u64();
        let new_nonce = self.new_header.nonce().as_canonical_u64();
        if new_nonce <= old_nonce {
            return Err(AccountDeltaError::AccountDeltaApplicationFailed {
                account_id: self.new_header.id(),
                source: AccountError::other(format!(
                    "node returned non-monotonic account nonce: local {old_nonce} >= new {new_nonce}"
                )),
            });
        }

        let storage_delta = replay_storage_updates(
            local_storage,
            &self.value_slot_updates,
            &self.storage_map_updates,
        )?;
        let vault_delta = replay_vault_updates(local_vault, &self.vault_updates)?;

        let nonce_delta = Felt::new(new_nonce - old_nonce).expect(
            "new_nonce was checked to be higher than old_nonce; should return a valid nonce",
        );

        AccountDelta::new(self.new_header.id(), storage_delta, vault_delta, nonce_delta)
    }
}

// DELTA REPLAY HELPERS
// ================================================================================================

/// Computes a storage delta by replaying incremental updates onto the locally-stored state.
fn replay_storage_updates(
    local_storage: &AccountStorage,
    value_slot_updates: &[(StorageSlotName, Word)],
    storage_map_updates: &[StorageMapUpdate],
) -> Result<AccountStorageDelta, AccountDeltaError> {
    let mut storage_delta = AccountStorageDelta::new();

    // Value slots: emit only the slots whose new value differs from local.
    for (slot_name, new_value) in value_slot_updates {
        let local_value = local_storage.get_item(slot_name).ok();
        if local_value.as_ref() != Some(new_value) {
            storage_delta.set_item(slot_name.clone(), *new_value)?;
        }
    }

    // Map slots: dedup updates per (slot, key) keeping the latest value by block number.
    let mut by_slot: BTreeMap<StorageSlotName, BTreeMap<StorageMapKey, Word>> = BTreeMap::new();
    let mut sorted: Vec<&StorageMapUpdate> = storage_map_updates.iter().collect();
    sorted.sort_by_key(|u| u.block_num);
    for update in sorted {
        by_slot
            .entry(update.slot_name.clone())
            .or_default()
            .insert(update.key, update.value);
    }
    for (slot_name, entries) in by_slot {
        for (key, value) in entries {
            storage_delta.set_map_item(slot_name.clone(), key, value)?;
        }
    }

    Ok(storage_delta)
}

/// Computes a vault delta by replaying incremental updates onto the locally-stored vault.
fn replay_vault_updates(
    local_vault: &AssetVault,
    vault_updates: &[AccountVaultUpdate],
) -> Result<AccountVaultDelta, AccountDeltaError> {
    let mut vault_delta = AccountVaultDelta::default();

    let mut final_vault: BTreeMap<AssetVaultKey, Asset> =
        local_vault.assets().map(|asset| (asset.vault_key(), asset)).collect();

    let mut sorted: Vec<&AccountVaultUpdate> = vault_updates.iter().collect();
    sorted.sort_by_key(|u| u.block_num);
    for update in sorted {
        match update.asset {
            Some(asset) => {
                final_vault.insert(update.vault_key, asset);
            },
            None => {
                final_vault.remove(&update.vault_key);
            },
        }
    }

    let local_assets: BTreeMap<AssetVaultKey, Asset> =
        local_vault.assets().map(|a| (a.vault_key(), a)).collect();
    for (key, final_asset) in &final_vault {
        match local_assets.get(key) {
            None => {
                vault_delta.add_asset(*final_asset)?;
            },
            Some(local_asset) if local_asset != final_asset => {
                vault_delta.remove_asset(*local_asset)?;
                vault_delta.add_asset(*final_asset)?;
            },
            _ => {},
        }
    }
    for (key, local_asset) in &local_assets {
        if !final_vault.contains_key(key) {
            vault_delta.remove_asset(*local_asset)?;
        }
    }

    Ok(vault_delta)
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
    use alloc::vec;

    use miden_protocol::account::{StorageMapKey, StorageSlot};
    use miden_protocol::asset::{Asset, AssetVault, FungibleAsset};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
    };

    use super::*;

    fn account_id() -> AccountId {
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap()
    }

    fn faucet_id() -> AccountId {
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into().unwrap()
    }

    fn slot_name(name: &str) -> StorageSlotName {
        StorageSlotName::new(name).unwrap()
    }

    fn map_key(n: u64) -> StorageMapKey {
        StorageMapKey::from_raw(word(n))
    }

    fn word(n: u64) -> Word {
        Word::from([
            Felt::new_unchecked(n),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
        ])
    }

    fn fungible(amount: u64) -> Asset {
        Asset::Fungible(FungibleAsset::new(faucet_id(), amount).unwrap())
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

    fn empty_payload(new_header: AccountHeader) -> PublicAccountDelta {
        PublicAccountDelta::new(
            new_header,
            BlockNumber::from(0u32),
            BlockNumber::from(1u32),
            vec![],
            vec![],
            vec![],
        )
    }

    // REPLAY STORAGE UPDATES
    // --------------------------------------------------------------------------------------------

    #[test]
    fn replay_storage_empty_inputs_returns_empty_delta() {
        let storage = AccountStorage::new(vec![]).unwrap();
        let delta = replay_storage_updates(&storage, &[], &[]).unwrap();
        assert!(delta.is_empty());
    }

    #[test]
    fn replay_storage_value_slot_changed_emits_delta() {
        let value_slot = slot_name("miden::test::value");
        let storage =
            AccountStorage::new(vec![StorageSlot::with_value(value_slot.clone(), word(1))])
                .unwrap();

        let delta =
            replay_storage_updates(&storage, &[(value_slot.clone(), word(2))], &[]).unwrap();

        let entry = delta.get(&value_slot).expect("delta should contain value slot");
        assert_eq!(entry.clone().unwrap_value(), word(2));
    }

    #[test]
    fn replay_storage_value_slot_unchanged_is_skipped() {
        let value_slot = slot_name("miden::test::value");
        let storage =
            AccountStorage::new(vec![StorageSlot::with_value(value_slot.clone(), word(1))])
                .unwrap();

        let delta =
            replay_storage_updates(&storage, &[(value_slot.clone(), word(1))], &[]).unwrap();

        assert!(delta.is_empty());
    }

    #[test]
    fn replay_storage_map_dedup_keeps_latest_block_per_key() {
        let map_slot = slot_name("miden::test::map");
        let storage =
            AccountStorage::new(vec![StorageSlot::with_empty_map(map_slot.clone())]).unwrap();

        let key = map_key(42);
        let updates = vec![
            StorageMapUpdate {
                block_num: BlockNumber::from(1u32),
                slot_name: map_slot.clone(),
                key,
                value: word(100),
            },
            StorageMapUpdate {
                block_num: BlockNumber::from(3u32),
                slot_name: map_slot.clone(),
                key,
                value: word(300),
            },
            StorageMapUpdate {
                block_num: BlockNumber::from(2u32),
                slot_name: map_slot.clone(),
                key,
                value: word(200),
            },
        ];

        let delta = replay_storage_updates(&storage, &[], &updates).unwrap();

        let map_delta = delta.get(&map_slot).expect("delta should contain map slot").clone();
        let map = map_delta.unwrap_map();
        assert_eq!(map.entries().len(), 1);
        assert_eq!(*map.entries().values().next().unwrap(), word(300));
    }

    #[test]
    fn replay_storage_map_multiple_keys_in_same_slot_all_kept() {
        let map_slot = slot_name("miden::test::map");
        let storage =
            AccountStorage::new(vec![StorageSlot::with_empty_map(map_slot.clone())]).unwrap();

        let updates = vec![
            StorageMapUpdate {
                block_num: BlockNumber::from(1u32),
                slot_name: map_slot.clone(),
                key: map_key(1),
                value: word(100),
            },
            StorageMapUpdate {
                block_num: BlockNumber::from(2u32),
                slot_name: map_slot.clone(),
                key: map_key(2),
                value: word(200),
            },
        ];

        let delta = replay_storage_updates(&storage, &[], &updates).unwrap();
        let map = delta.get(&map_slot).unwrap().clone().unwrap_map();
        assert_eq!(map.entries().len(), 2);
    }

    // REPLAY VAULT UPDATES
    // --------------------------------------------------------------------------------------------

    #[test]
    fn replay_vault_empty_inputs_returns_empty_delta() {
        let vault = AssetVault::new(&[]).unwrap();
        let delta = replay_vault_updates(&vault, &[]).unwrap();
        assert!(delta.is_empty());
    }

    #[test]
    fn replay_vault_added_asset_emits_add() {
        let vault = AssetVault::new(&[]).unwrap();
        let asset = fungible(100);
        let updates = vec![AccountVaultUpdate {
            block_num: BlockNumber::from(1u32),
            asset: Some(asset),
            vault_key: asset.vault_key(),
        }];

        let delta = replay_vault_updates(&vault, &updates).unwrap();
        let added: Vec<_> = delta.added_assets().collect();
        assert_eq!(added, vec![asset]);
        assert_eq!(delta.removed_assets().count(), 0);
    }

    #[test]
    fn replay_vault_removed_asset_emits_remove() {
        let asset = fungible(100);
        let vault = AssetVault::new(&[asset]).unwrap();
        let updates = vec![AccountVaultUpdate {
            block_num: BlockNumber::from(1u32),
            asset: None,
            vault_key: asset.vault_key(),
        }];

        let delta = replay_vault_updates(&vault, &updates).unwrap();
        let removed: Vec<_> = delta.removed_assets().collect();
        assert_eq!(removed, vec![asset]);
        assert_eq!(delta.added_assets().count(), 0);
    }

    #[test]
    fn replay_vault_replace_asset_emits_net_diff() {
        let asset_a = fungible(100);
        let asset_b = fungible(150);
        let vault = AssetVault::new(&[asset_a]).unwrap();
        let updates = vec![AccountVaultUpdate {
            block_num: BlockNumber::from(1u32),
            asset: Some(asset_b),
            vault_key: asset_b.vault_key(),
        }];

        let delta = replay_vault_updates(&vault, &updates).unwrap();
        let added: Vec<_> = delta.added_assets().collect();
        assert_eq!(added, vec![fungible(50)]);
        assert_eq!(delta.removed_assets().count(), 0);
    }

    #[test]
    fn replay_vault_dedup_keeps_latest_block_per_key() {
        let vault = AssetVault::new(&[]).unwrap();
        let asset_v1 = fungible(100);
        let asset_v2 = fungible(200);
        let asset_v3 = fungible(300);
        let key = asset_v1.vault_key();

        let updates = vec![
            AccountVaultUpdate {
                block_num: BlockNumber::from(1u32),
                asset: Some(asset_v1),
                vault_key: key,
            },
            AccountVaultUpdate {
                block_num: BlockNumber::from(3u32),
                asset: Some(asset_v3),
                vault_key: key,
            },
            AccountVaultUpdate {
                block_num: BlockNumber::from(2u32),
                asset: Some(asset_v2),
                vault_key: key,
            },
        ];

        let delta = replay_vault_updates(&vault, &updates).unwrap();
        let added: Vec<_> = delta.added_assets().collect();
        assert_eq!(added, vec![asset_v3]);
    }

    #[test]
    fn replay_vault_added_then_removed_is_noop() {
        let vault = AssetVault::new(&[]).unwrap();
        let asset = fungible(100);
        let key = asset.vault_key();

        let updates = vec![
            AccountVaultUpdate {
                block_num: BlockNumber::from(1u32),
                asset: Some(asset),
                vault_key: key,
            },
            AccountVaultUpdate {
                block_num: BlockNumber::from(2u32),
                asset: None,
                vault_key: key,
            },
        ];

        let delta = replay_vault_updates(&vault, &updates).unwrap();
        assert!(delta.is_empty());
    }

    // COMPUTE ACCOUNT DELTA
    // --------------------------------------------------------------------------------------------

    #[test]
    fn compute_delta_happy_path_emits_nonce_delta() {
        let local_header = header_with_nonce(1);
        let local_storage = AccountStorage::new(vec![]).unwrap();
        let local_vault = AssetVault::new(&[]).unwrap();
        let payload = empty_payload(header_with_nonce(4));

        let delta = payload
            .compute_account_delta(&local_header, &local_storage, &local_vault)
            .unwrap();

        assert_eq!(delta.nonce_delta(), Felt::new_unchecked(3));
        assert!(delta.storage().is_empty());
        assert!(delta.vault().is_empty());
    }

    #[test]
    fn compute_delta_rejects_equal_nonce() {
        let local_header = header_with_nonce(5);
        let local_storage = AccountStorage::new(vec![]).unwrap();
        let local_vault = AssetVault::new(&[]).unwrap();
        let payload = empty_payload(header_with_nonce(5));

        let err = payload
            .compute_account_delta(&local_header, &local_storage, &local_vault)
            .unwrap_err();

        assert!(matches!(
            err,
            AccountDeltaError::AccountDeltaApplicationFailed {
                source: AccountError::Other { .. },
                ..
            }
        ));
    }

    #[test]
    fn compute_delta_rejects_decreasing_nonce() {
        let local_header = header_with_nonce(10);
        let local_storage = AccountStorage::new(vec![]).unwrap();
        let local_vault = AssetVault::new(&[]).unwrap();
        let payload = empty_payload(header_with_nonce(9));

        let err = payload
            .compute_account_delta(&local_header, &local_storage, &local_vault)
            .unwrap_err();

        assert!(matches!(
            err,
            AccountDeltaError::AccountDeltaApplicationFailed {
                source: AccountError::Other { .. },
                ..
            }
        ));
    }
}

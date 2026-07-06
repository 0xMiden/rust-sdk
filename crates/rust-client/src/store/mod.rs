//! Defines the storage interfaces used by the Miden client.
//!
//! It provides mechanisms for persisting and retrieving data, such as account states, transaction
//! history, block headers, notes, and MMR nodes.
//!
//! ## Overview
//!
//! The storage module is central to the Miden client’s persistence layer. It defines the
//! [`Store`] trait which abstracts over any concrete storage implementation. The trait exposes
//! methods to (among others):
//!
//! - Retrieve and update transactions, notes, and accounts.
//! - Store and query block headers along with MMR peaks and authentication nodes.
//! - Manage note tags for synchronizing with the node.
//!
//! These are all used by the Miden client to provide transaction execution in the correct contexts.
//!
//! In addition to the main [`Store`] trait, the module provides types for filtering queries, such
//! as [`TransactionFilter`], [`NoteFilter`], `StorageFilter` to narrow down the set of returned
//! transactions, account data, or notes. For more advanced usage, see the documentation of
//! individual methods in the [`Store`] trait.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Debug;

use miden_protocol::account::{
    Account,
    AccountCode,
    AccountHeader,
    AccountId,
    AccountStorage,
    StorageMapKey,
    StorageMapWitness,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
};
use miden_protocol::address::Address;
use miden_protocol::asset::{Asset, AssetVault, AssetVaultKey, AssetWitness};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::{Forest, InOrderIndex, MmrPeaks, PartialMmr};
use miden_protocol::errors::AccountError;
use miden_protocol::note::{NoteDetailsCommitment, NoteId, NoteScript, NoteTag, Nullifier};
use miden_protocol::transaction::TransactionId;
use miden_protocol::{Felt, Word};
use miden_tx::utils::serde::{Deserializable, Serializable};

use crate::note_transport::{NOTE_TRANSPORT_CURSOR_STORE_SETTING, NoteTransportCursor};
use crate::rpc::{RPC_LIMITS_STORE_SETTING, RpcLimits};
use crate::sync::{NoteTagRecord, StateSyncUpdate};
use crate::transaction::{TransactionRecord, TransactionStatusVariant, TransactionStoreUpdate};

/// Contains [`ClientDataStore`] to automatically implement [`DataStore`] for anything that
/// implements [`Store`]. This isn't public because it's an implementation detail to instantiate the
/// executor.
///
/// The user is tasked with creating a [`Store`] which the client will wrap into a
/// [`ClientDataStore`] at creation time.
pub(crate) mod data_store;

mod errors;
pub use errors::*;

mod smt_forest;
pub use smt_forest::AccountSmtForest;

mod account;
pub use account::{
    AccountRecord,
    AccountRecordData,
    AccountStatus,
    AccountUpdates,
    ClientAccountType,
};

pub use crate::sync::PublicAccountUpdate;
mod note_record;
pub use note_record::{
    InputNoteRecord,
    InputNoteState,
    NoteExportType,
    NoteRecordError,
    OutputNoteRecord,
    OutputNoteState,
    input_note_states,
};

// SETTING MUTATION
// ================================================================================================

/// A single mutation against the `settings` KV store, applied as part of an atomic batch via
/// [`Store::apply_settings_mutations`].
#[derive(Debug, Clone)]
pub enum SettingMutation {
    /// Insert or overwrite `key` with `value`.
    Set { key: String, value: Vec<u8> },
    /// Delete `key`.
    Remove { key: String },
}

// STORE TRAIT
// ================================================================================================

/// The [`Store`] trait exposes all methods that the client store needs in order to track the
/// current state.
///
/// All update functions are implied to be atomic. That is, if multiple entities are meant to be
/// updated as part of any single function and an error is returned during its execution, any
/// changes that might have happened up to that point need to be rolled back and discarded.
///
/// Because the [`Store`]'s ownership is shared between the executor and the client, interior
/// mutability is expected to be implemented, which is why all methods receive `&self` and
/// not `&mut self`.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Store: Send + Sync {
    /// Returns an identifier for this store (e.g. `IndexedDB` database name, `SQLite` file path).
    ///
    /// This allows callers to retrieve store-specific identity information (such as the `IndexedDB`
    /// database name) for standalone operations like `exportStore`/`importStore`, without making
    /// import/export a responsibility of the client.
    fn identifier(&self) -> &str;

    /// Returns the current timestamp tracked by the store, measured in non-leap seconds since
    /// Unix epoch. If the store implementation is incapable of tracking time, it should return
    /// `None`.
    ///
    /// This method is used to add time metadata to notes' states. This information doesn't have a
    /// functional impact on the client's operation, it's shown to the user for informational
    /// purposes.
    fn get_current_timestamp(&self) -> Option<u64>;

    // TRANSACTIONS
    // --------------------------------------------------------------------------------------------

    /// Retrieves stored transactions, filtered by [`TransactionFilter`].
    async fn get_transactions(
        &self,
        filter: TransactionFilter,
    ) -> Result<Vec<TransactionRecord>, StoreError>;

    /// Applies a transaction, atomically updating the current state based on the
    /// [`TransactionStoreUpdate`].
    ///
    /// An update involves:
    /// - Updating the stored account which is being modified by the transaction.
    /// - Storing new input/output notes and payback note details as a result of the transaction
    ///   execution.
    /// - Updating the input notes that are being processed by the transaction.
    /// - Inserting the new tracked tags into the store.
    /// - Inserting the transaction into the store to track.
    async fn apply_transaction(&self, tx_update: TransactionStoreUpdate) -> Result<(), StoreError>;

    /// Applies a batch of [`TransactionStoreUpdate`]s atomically. Semantically equivalent to
    /// calling [`Store::apply_transaction`] for each update in order, but with an all-or-nothing
    /// guarantee — on any error no update is visible.
    ///
    /// Used by `BatchBuilder::submit` to persist a batch's results. Backends that cannot provide
    /// true atomicity must document that limitation explicitly in their impl — there is no blanket
    /// default.
    async fn apply_transaction_batch(
        &self,
        tx_updates: Vec<TransactionStoreUpdate>,
    ) -> Result<(), StoreError>;

    // NOTES
    // --------------------------------------------------------------------------------------------

    /// Retrieves the input notes from the store.
    ///
    /// When `filter` is [`NoteFilter::Consumed`], notes are sorted by their on-chain execution
    /// order.
    async fn get_input_notes(&self, filter: NoteFilter)
    -> Result<Vec<InputNoteRecord>, StoreError>;

    /// Retrieves the output notes from the store.
    async fn get_output_notes(
        &self,
        filter: NoteFilter,
    ) -> Result<Vec<OutputNoteRecord>, StoreError>;

    /// Retrieves a single input note at the given offset from the filtered set for the given
    /// consumer account. Optionally restricts to a block range via `block_start` and
    /// `block_end`. Returns `None` when the offset is past the end of the matching notes.
    ///
    /// # Ordering
    ///
    /// Notes are sorted by their per-account on-chain execution order.
    async fn get_input_note_by_offset(
        &self,
        filter: NoteFilter,
        consumer: AccountId,
        block_start: Option<BlockNumber>,
        block_end: Option<BlockNumber>,
        offset: u32,
    ) -> Result<Option<InputNoteRecord>, StoreError>;

    /// Returns the nullifiers of all unspent input notes.
    ///
    /// The default implementation of this method uses [`Store::get_input_notes`].
    async fn get_unspent_input_note_nullifiers(&self) -> Result<Vec<Nullifier>, StoreError> {
        Ok(self
            .get_input_notes(NoteFilter::Unspent)
            .await?
            .iter()
            .filter_map(InputNoteRecord::nullifier)
            .collect())
    }

    /// Inserts the provided input notes into the database. If a note with the same ID already
    /// exists, it will be replaced.
    async fn upsert_input_notes(&self, notes: &[InputNoteRecord]) -> Result<(), StoreError>;

    /// Returns the note script associated with the given root.
    async fn get_note_script(&self, script_root: Word) -> Result<NoteScript, StoreError>;

    /// Inserts the provided note scripts into the database. If a script with the same root already
    /// exists, it will be replaced.
    async fn upsert_note_scripts(&self, note_scripts: &[NoteScript]) -> Result<(), StoreError>;

    // CHAIN DATA
    // --------------------------------------------------------------------------------------------

    /// Retrieves a vector of [`BlockHeader`]s filtered by the provided block numbers.
    ///
    /// The returned vector may not contain some or all of the requested block headers. It's up to
    /// the callee to check whether all requested block headers were found.
    ///
    /// For each block header an additional boolean value is returned representing whether the block
    /// contains notes relevant to the client.
    async fn get_block_headers(
        &self,
        block_numbers: &BTreeSet<BlockNumber>,
    ) -> Result<Vec<(BlockHeader, BlockRelevance)>, StoreError>;

    /// Retrieves a [`BlockHeader`] corresponding to the provided block number and a boolean value
    /// that represents whether the block contains notes relevant to the client. Returns `None` if
    /// the block is not found.
    ///
    /// The default implementation of this method uses [`Store::get_block_headers`].
    async fn get_block_header_by_num(
        &self,
        block_number: BlockNumber,
    ) -> Result<Option<(BlockHeader, BlockRelevance)>, StoreError> {
        self.get_block_headers(&[block_number].into_iter().collect())
            .await
            .map(|mut block_headers_list| block_headers_list.pop())
    }

    /// Retrieves a list of [`BlockHeader`] that include relevant notes to the client.
    async fn get_tracked_block_headers(&self) -> Result<Vec<BlockHeader>, StoreError>;

    /// Retrieves the block numbers of block headers that include relevant notes to the client.
    ///
    /// This is a lightweight alternative to [`Store::get_tracked_block_headers`] that avoids
    /// deserializing full block headers when only the block numbers are needed.
    async fn get_tracked_block_header_numbers(&self) -> Result<BTreeSet<usize>, StoreError>;

    /// Retrieves all MMR authentication nodes based on [`PartialBlockchainFilter`].
    async fn get_partial_blockchain_nodes(
        &self,
        filter: PartialBlockchainFilter,
    ) -> Result<BTreeMap<InOrderIndex, Word>, StoreError>;

    /// Returns the chain MMR peaks at the current sync height (peaks at `forest = block_num`,
    /// i.e. excluding `block_num` itself as a leaf).
    ///
    /// The peaks' `forest().num_leaves()` equals the current sync height by construction,
    /// so callers can derive the synced block number from the returned peaks without a
    /// second query.
    ///
    /// Before the first sync, returns an empty [`MmrPeaks`].
    async fn get_current_blockchain_peaks(&self) -> Result<MmrPeaks, StoreError>;

    /// Inserts a block header together with its MMR authentication nodes in a single
    /// transaction, so the header and the nodes that rebuild its `PartialMmr` are committed
    /// together.
    ///
    /// The header is inserted-if-not-exists with a one-way `has_client_notes` upgrade: on
    /// conflict the stored `header` is preserved and the flag only moves from `false` to
    /// `true`, never back. The MMR nodes are likewise inserted-if-not-exists: an
    /// `InOrderIndex` already present is left untouched (auth paths of tracked blocks share
    /// internal nodes, so re-inserting an existing index must be a no-op, not an error).
    async fn insert_block_header(
        &self,
        block_header: &BlockHeader,
        nodes: &[(InOrderIndex, Word)],
        has_client_notes: bool,
    ) -> Result<(), StoreError>;

    /// Prunes irrelevant block data from the store.
    ///
    /// This performs three operations atomically:
    /// 1. Deletes MMR authentication nodes at the given `node_indices`.
    /// 2. Sets `has_client_notes = false` for `blocks_to_untrack` (blocks whose notes have all been
    ///    consumed).
    /// 3. Deletes block headers with `has_client_notes = false` that are not the genesis or
    ///    sync-height block.
    async fn untrack_and_prune_irrelevant_blocks(
        &self,
        blocks_to_untrack: &[BlockNumber],
        node_indices_to_remove: &[InOrderIndex],
    ) -> Result<(), StoreError>;

    /// Prunes historical account states for the specified account up to the given nonce.
    ///
    /// Deletes all historical entries with `replaced_at_nonce <= up_to_nonce` from the
    /// historical tables (headers, storage, storage map entries, and assets).
    ///
    /// Also removes orphaned `account_code` entries that are no longer referenced by any
    /// account header.
    ///
    /// Returns the total number of rows deleted, including historical entries and orphaned
    /// account code.
    async fn prune_account_history(
        &self,
        account_id: AccountId,
        up_to_nonce: Felt,
    ) -> Result<usize, StoreError>;

    // ACCOUNT
    // --------------------------------------------------------------------------------------------

    /// Returns the account IDs of all accounts stored in the database.
    async fn get_account_ids(&self) -> Result<Vec<AccountId>, StoreError>;

    /// Returns a list of [`AccountHeader`] of all accounts stored in the database along with their
    /// statuses.
    ///
    /// Said accounts' state is the state after the last performed sync.
    async fn get_account_headers(&self) -> Result<Vec<(AccountHeader, AccountStatus)>, StoreError>;

    /// Retrieves an [`AccountHeader`] object for the specified [`AccountId`] along with its status.
    /// Returns `None` if the account is not found.
    ///
    /// Said account's state is the state according to the last sync performed.
    async fn get_account_header(
        &self,
        account_id: AccountId,
    ) -> Result<Option<(AccountHeader, AccountStatus)>, StoreError>;

    /// Returns an [`AccountHeader`] corresponding to the stored account state that matches the
    /// given commitment. If no account state matches the provided commitment, `None` is returned.
    async fn get_account_header_by_commitment(
        &self,
        account_commitment: Word,
    ) -> Result<Option<AccountHeader>, StoreError>;

    /// Retrieves a full [`AccountRecord`] object, this contains the account's latest state along
    /// with its status. Returns `None` if the account is not found.
    async fn get_account(&self, account_id: AccountId)
    -> Result<Option<AccountRecord>, StoreError>;

    /// Retrieves the [`AccountCode`] for the specified account.
    /// Returns `None` if the account is not found.
    async fn get_account_code(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountCode>, StoreError>;

    /// Inserts an [`Account`] to the store, alongside its initial [`Address`].
    ///
    /// Tag registration is the caller's responsibility — see [`Self::add_note_tag`].
    ///
    /// # Errors
    ///
    /// - If the account is new and does not contain a seed
    async fn insert_account(
        &self,
        account: &Account,
        initial_address: Address,
        client_account_type: ClientAccountType,
    ) -> Result<(), StoreError>;

    /// Upserts the account code for a foreign account. This value will be used as a cache of known
    /// script roots and added to the `GetForeignAccountCode` request.
    async fn upsert_foreign_account_code(
        &self,
        account_id: AccountId,
        code: AccountCode,
    ) -> Result<(), StoreError>;

    /// Retrieves the cached account code for various foreign accounts.
    async fn get_foreign_account_code(
        &self,
        account_ids: Vec<AccountId>,
    ) -> Result<BTreeMap<AccountId, AccountCode>, StoreError>;

    /// Retrieves all [`Address`] objects that correspond to the provided account ID.
    async fn get_addresses_by_account_id(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<Address>, StoreError>;

    /// Updates an existing [`Account`] with a new state.
    ///
    /// # Errors
    ///
    /// Returns a `StoreError::AccountDataNotFound` if there is no account for the provided ID.
    async fn update_account(&self, new_account_state: &Account) -> Result<(), StoreError>;

    /// Adds an [`Address`] to an [`Account`].
    ///
    /// Tag registration is the caller's responsibility — see [`Self::add_note_tag`].
    async fn insert_address(
        &self,
        address: Address,
        account_id: AccountId,
    ) -> Result<(), StoreError>;

    /// Removes an [`Address`].
    ///
    /// Tag removal is the caller's responsibility — see [`Self::remove_note_tag`].
    async fn remove_address(&self, address: Address) -> Result<(), StoreError>;

    // SETTINGS
    // --------------------------------------------------------------------------------------------

    /// Adds a value to the `settings` table.
    async fn set_setting(&self, key: String, value: Vec<u8>) -> Result<(), StoreError>;

    /// Retrieves a value from the `settings` table.
    async fn get_setting(&self, key: String) -> Result<Option<Vec<u8>>, StoreError>;

    /// Deletes a value from the `settings` table.
    async fn remove_setting(&self, key: String) -> Result<(), StoreError>;

    /// Returns all the keys from the `settings` table.
    async fn list_setting_keys(&self) -> Result<Vec<String>, StoreError>;

    /// Applies a batch of [`SettingMutation`]s. Use this when several `settings` entries must stay
    /// mutually consistent (e.g. a record and its secondary index).
    async fn apply_settings_mutations(
        &self,
        mutations: Vec<SettingMutation>,
    ) -> Result<(), StoreError>;

    // SYNC
    // --------------------------------------------------------------------------------------------

    /// Returns the note tag records that the client is interested in.
    async fn get_note_tags(&self) -> Result<Vec<NoteTagRecord>, StoreError>;

    /// Returns the unique note tags (without source) that the client is interested in.
    async fn get_unique_note_tags(&self) -> Result<BTreeSet<NoteTag>, StoreError> {
        Ok(self.get_note_tags().await?.into_iter().map(|r| r.tag).collect())
    }

    /// Adds a note tag to the list of tags that the client is interested in.
    ///
    /// If the tag was already being tracked, returns false since no new tags were actually added.
    /// Otherwise true.
    async fn add_note_tag(&self, tag: NoteTagRecord) -> Result<bool, StoreError>;

    /// Removes a note tag from the list of tags that the client is interested in.
    ///
    /// If the tag wasn't present in the store returns false since no tag was actually removed.
    /// Otherwise returns true.
    async fn remove_note_tag(&self, tag: NoteTagRecord) -> Result<usize, StoreError>;

    /// Returns the block number of the last state sync block.
    async fn get_sync_height(&self) -> Result<BlockNumber, StoreError>;

    /// Applies the state sync update to the store. An update involves:
    ///
    /// - Inserting the new block header to the store alongside new MMR peaks information.
    /// - Updating the corresponding tracked input/output notes. Consumed notes carry consumption
    ///   metadata — `consumed_block_height`, `consumed_tx_order`, and `consumer_account_id` — in
    ///   their note state. Implementations must persist these fields so that ordered queries (see
    ///   [`Store::get_input_note_by_offset`]) work correctly.
    /// - Removing note tags that are no longer relevant.
    /// - Updating transactions in the store, marking as `committed` or `discarded`.
    ///   - In turn, validating private account's state transitions. If a private account's
    ///     commitment locally does not match the `StateSyncUpdate` information, the account may be
    ///     locked.
    /// - Storing new MMR authentication nodes.
    /// - Updating the tracked public accounts.
    async fn apply_state_sync(&self, state_sync_update: StateSyncUpdate) -> Result<(), StoreError>;

    // TRANSPORT
    // --------------------------------------------------------------------------------------------

    /// Gets the note transport cursor.
    ///
    /// This is used to reduce the number of fetched notes from the note transport network.
    /// If no cursor exists, initializes it to 0.
    async fn get_note_transport_cursor(&self) -> Result<NoteTransportCursor, StoreError> {
        let cursor_bytes = if let Some(bytes) =
            self.get_setting(NOTE_TRANSPORT_CURSOR_STORE_SETTING.into()).await?
        {
            bytes
        } else {
            // Lazy initialization: create cursor if not present
            let initial = 0u64.to_be_bytes().to_vec();
            self.set_setting(NOTE_TRANSPORT_CURSOR_STORE_SETTING.into(), initial.clone())
                .await?;
            initial
        };
        let array: [u8; 8] = cursor_bytes
            .as_slice()
            .try_into()
            .map_err(|e: core::array::TryFromSliceError| StoreError::ParsingError(e.to_string()))?;
        let cursor = u64::from_be_bytes(array);
        Ok(cursor.into())
    }

    /// Updates the note transport cursor.
    ///
    /// This is used to track the last cursor position when fetching notes from the note transport
    /// network.
    async fn update_note_transport_cursor(
        &self,
        cursor: NoteTransportCursor,
    ) -> Result<(), StoreError> {
        let cursor_bytes = cursor.value().to_be_bytes().to_vec();
        self.set_setting(NOTE_TRANSPORT_CURSOR_STORE_SETTING.into(), cursor_bytes)
            .await?;
        Ok(())
    }

    // RPC LIMITS
    // --------------------------------------------------------------------------------------------

    /// Gets persisted RPC limits. Returns `None` if not stored.
    async fn get_rpc_limits(&self) -> Result<Option<RpcLimits>, StoreError> {
        let Some(bytes) = self.get_setting(RPC_LIMITS_STORE_SETTING.into()).await? else {
            return Ok(None);
        };
        let limits = RpcLimits::read_from_bytes(&bytes)?;
        Ok(Some(limits))
    }

    /// Persists RPC limits to the store.
    async fn set_rpc_limits(&self, limits: RpcLimits) -> Result<(), StoreError> {
        self.set_setting(RPC_LIMITS_STORE_SETTING.into(), limits.to_bytes()).await
    }

    // PARTIAL MMR
    // --------------------------------------------------------------------------------------------

    /// Builds the current view of the chain's [`PartialMmr`]. Because we want to add all new
    /// authentication nodes that could come from applying the MMR updates, we need to track all
    /// known leaves thus far.
    ///
    /// The default implementation is based on [`Store::get_partial_blockchain_nodes`],
    /// [`Store::get_current_blockchain_peaks`] and [`Store::get_block_header_by_num`]
    async fn get_current_partial_mmr(&self) -> Result<PartialMmr, StoreError> {
        let current_peaks = self.get_current_blockchain_peaks().await?;
        let current_block_num = u32::try_from(current_peaks.num_leaves())
            .map_err(|err| StoreError::ParsingError(err.to_string()))?
            .into();

        let (current_block, has_client_notes) = self
            .get_block_header_by_num(current_block_num)
            .await?
            .ok_or(StoreError::BlockHeaderNotFound(current_block_num))?;

        let mut current_partial_mmr = PartialMmr::from_peaks(current_peaks);
        let has_client_notes = has_client_notes.into();
        current_partial_mmr
            .add(current_block.commitment(), has_client_notes)
            .map_err(StoreError::MmrError)?;

        // Build tracked_leaves from blocks that have client notes.
        let mut tracked_leaves = self.get_tracked_block_header_numbers().await?;

        // Also track the latest leaf if it is relevant (it has client notes) _and_ the forest
        // actually has a single leaf tree bit.
        if has_client_notes && current_partial_mmr.forest().has_single_leaf_tree() {
            let latest_leaf = current_partial_mmr.forest().num_leaves().saturating_sub(1);
            tracked_leaves.insert(latest_leaf);
        }

        let tracked_nodes = self
            .get_partial_blockchain_nodes(PartialBlockchainFilter::Forest(
                current_partial_mmr.forest(),
            ))
            .await?;

        let current_partial_mmr =
            PartialMmr::from_parts(current_partial_mmr.peaks(), tracked_nodes, tracked_leaves)?;

        Ok(current_partial_mmr)
    }

    // ACCOUNT VAULT AND STORE
    // --------------------------------------------------------------------------------------------

    /// Retrieves the asset vault for a specific account.
    async fn get_account_vault(&self, account_id: AccountId) -> Result<AssetVault, StoreError>;

    /// Retrieves a specific asset (by vault key) from the account's vault along with its Merkle
    /// witness.
    ///
    /// The default implementation of this method uses [`Store::get_account_vault`].
    async fn get_account_asset(
        &self,
        account_id: AccountId,
        vault_key: AssetVaultKey,
    ) -> Result<Option<(Asset, AssetWitness)>, StoreError> {
        let vault = self.get_account_vault(account_id).await?;
        let Some(asset) = vault.assets().find(|a| a.vault_key() == vault_key) else {
            return Ok(None);
        };

        let witness = AssetWitness::new(vault.open(vault_key).into())?;

        Ok(Some((asset, witness)))
    }

    /// Retrieves the storage for a specific account.
    ///
    /// Can take an optional map root to retrieve only part of the storage,
    /// If it does, it will either return an account storage with a single
    /// slot (the one requested), or an error if not found.
    async fn get_account_storage(
        &self,
        account_id: AccountId,
        filter: AccountStorageFilter,
    ) -> Result<AccountStorage, StoreError>;

    /// Retrieves a storage slot value by name.
    ///
    /// For `Value` slots, returns the stored word.
    /// For `Map` slots, returns the map root.
    ///
    /// The default implementation of this method uses [`Store::get_account_storage`].
    async fn get_account_storage_item(
        &self,
        account_id: AccountId,
        slot_name: StorageSlotName,
    ) -> Result<Word, StoreError> {
        let storage = self
            .get_account_storage(account_id, AccountStorageFilter::SlotName(slot_name.clone()))
            .await?;
        storage
            .get(&slot_name)
            .map(StorageSlot::value)
            .ok_or(StoreError::AccountError(AccountError::StorageSlotNameNotFound { slot_name }))
    }

    /// Retrieves a specific item from the account's storage map along with its Merkle proof.
    ///
    /// The default implementation of this method uses [`Store::get_account_storage`].
    async fn get_account_map_item(
        &self,
        account_id: AccountId,
        slot_name: StorageSlotName,
        key: StorageMapKey,
    ) -> Result<(Word, StorageMapWitness), StoreError> {
        let storage = self
            .get_account_storage(account_id, AccountStorageFilter::SlotName(slot_name.clone()))
            .await?;
        match storage.get(&slot_name).map(StorageSlot::content) {
            Some(StorageSlotContent::Map(map)) => {
                let value = map.get(&key);
                let witness = map.open(&key);

                Ok((value, witness))
            },
            Some(_) => Err(StoreError::AccountError(AccountError::StorageSlotNotMap(slot_name))),
            None => {
                Err(StoreError::AccountError(AccountError::StorageSlotNameNotFound { slot_name }))
            },
        }
    }

    // PARTIAL ACCOUNTS
    // --------------------------------------------------------------------------------------------

    /// Retrieves an [`AccountRecord`] object, this contains the account's latest partial
    /// state along with its status. Returns `None` if the partial account is not found.
    async fn get_minimal_partial_account(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountRecord>, StoreError>;
}

// PARTIAL BLOCKCHAIN NODE FILTER
// ================================================================================================

/// Filters for searching specific MMR nodes.
// TODO: Should there be filters for specific blocks instead of nodes?
pub enum PartialBlockchainFilter {
    /// Return all nodes.
    All,
    /// Filter by the specified in-order indices.
    List(Vec<InOrderIndex>),
    /// Return nodes with in-order indices within the specified forest.
    Forest(Forest),
}

// TRANSACTION FILTERS
// ================================================================================================

/// Filters for narrowing the set of transactions returned by the client's store.
#[derive(Debug, Clone)]
pub enum TransactionFilter {
    /// Return all transactions.
    All,
    /// Filter by transactions that haven't yet been committed to the blockchain as per the last
    /// sync.
    Uncommitted,
    /// Return a list of the transaction that matches the provided [`TransactionId`]s.
    Ids(Vec<TransactionId>),
    /// Return a list of the expired transactions that were executed before the provided
    /// [`BlockNumber`]. Transactions created after the provided block number are not
    /// considered.
    ///
    /// A transaction is considered expired if is uncommitted and the transaction's block number
    /// is less than the provided block number.
    ExpiredBefore(BlockNumber),
}

// TRANSACTIONS FILTER HELPERS
// ================================================================================================

impl TransactionFilter {
    /// Returns a [String] containing the query for this Filter.
    pub fn to_query(&self) -> String {
        const QUERY: &str = "SELECT tx.id, script.script, tx.details, tx.status \
            FROM transactions AS tx LEFT JOIN transaction_scripts AS script ON tx.script_root = script.script_root";
        match self {
            TransactionFilter::All => QUERY.to_string(),
            TransactionFilter::Uncommitted => format!(
                "{QUERY} WHERE tx.status_variant = {}",
                TransactionStatusVariant::Pending as u8,
            ),
            TransactionFilter::Ids(_) => {
                // Use SQLite's array parameter binding
                format!("{QUERY} WHERE tx.id IN rarray(?)")
            },
            TransactionFilter::ExpiredBefore(block_num) => {
                format!(
                    "{QUERY} WHERE tx.block_num < {} AND tx.status_variant != {} AND tx.status_variant != {}",
                    block_num.as_u32(),
                    TransactionStatusVariant::Discarded as u8,
                    TransactionStatusVariant::Committed as u8
                )
            },
        }
    }
}

// NOTE FILTER
// ================================================================================================

/// Filters for narrowing the set of notes returned by the client's store.
#[derive(Debug, Clone)]
pub enum NoteFilter {
    /// Return a list of all notes ([`InputNoteRecord`] or [`OutputNoteRecord`]).
    All,
    /// Return a list of committed notes ([`InputNoteRecord`] or [`OutputNoteRecord`]). These
    /// represent notes that the blockchain has included in a block.
    Committed,
    /// Filter by consumed notes ([`InputNoteRecord`] or [`OutputNoteRecord`]). notes that have
    /// been used as inputs in transactions.
    Consumed,
    /// Return a list of expected notes ([`InputNoteRecord`] or [`OutputNoteRecord`]). These
    /// represent notes for which the store doesn't have anchor data.
    Expected,
    /// Return a list containing any notes that match with the provided [`NoteId`] vector.
    List(Vec<NoteId>),
    /// Return a list containing any notes whose details commitment matches one of the provided
    /// [`NoteDetailsCommitment`] vector. Unlike [`NoteFilter::List`], this matches the
    /// metadata-independent details commitment, so it also resolves metadata-less notes (which
    /// have a NULL `note_id`).
    DetailsCommitments(Vec<NoteDetailsCommitment>),
    /// Return a list containing any notes that match the provided [`Nullifier`] vector.
    Nullifiers(Vec<Nullifier>),
    /// Return a list of notes that are currently being processed. This filter doesn't apply to
    /// output notes.
    Processing,
    /// Return a list containing the note that matches with the provided [`NoteId`]. The query will
    /// return an error if the note isn't found.
    Unique(NoteId),
    /// Return a list containing notes that haven't been nullified yet, this includes expected,
    /// committed, processing and unverified notes.
    Unspent,
    /// Return a list containing notes with unverified inclusion proofs. This filter doesn't apply
    /// to output notes.
    Unverified,
}

// BLOCK RELEVANCE
// ================================================================================================

/// Expresses metadata about the block header.
#[derive(Debug, Clone)]
pub enum BlockRelevance {
    /// The block header includes notes that the client may consume.
    HasNotes,
    /// The block header does not contain notes relevant to the client.
    Irrelevant,
}

impl From<BlockRelevance> for bool {
    fn from(val: BlockRelevance) -> Self {
        match val {
            BlockRelevance::HasNotes => true,
            BlockRelevance::Irrelevant => false,
        }
    }
}

impl From<bool> for BlockRelevance {
    fn from(has_notes: bool) -> Self {
        if has_notes {
            BlockRelevance::HasNotes
        } else {
            BlockRelevance::Irrelevant
        }
    }
}

// STORAGE FILTER
// ================================================================================================

/// Filters for narrowing the storage slots returned by the client's store.
#[derive(Debug, Clone)]
pub enum AccountStorageFilter {
    /// Return an [`AccountStorage`] with all available slots.
    All,
    /// Return an [`AccountStorage`] with a single slot that matches the provided [`Word`] map root.
    Root(Word),
    /// Return an [`AccountStorage`] with a single slot that matches the provided slot name.
    SlotName(StorageSlotName),
    /// Return an [`AccountStorage`] containing only the slots whose names are in the provided
    /// list. Useful to avoid loading the full storage when only a known subset of slots is needed
    /// (e.g. when applying a delta to a large account).
    SlotNames(Vec<StorageSlotName>),
}

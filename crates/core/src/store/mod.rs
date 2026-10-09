//! Defines the storage contract shared by the client and the store implementations.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

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
use miden_protocol::asset::{Asset, AssetId, AssetVault, AssetWitness};
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, MmrPeaks, PartialMmr};
use miden_protocol::errors::AccountError;
use miden_protocol::note::{NoteScript, NoteTag, Nullifier};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Felt, Word};

#[allow(deprecated)]
use crate::note_transport::{NOTE_TRANSPORT_CURSOR_STORE_SETTING, NoteTransportCursor};
use crate::rpc::encryption::{TRANSACTION_ENCRYPTION_KEY_STORE_SETTING, TransactionEncryptionKey};
use crate::rpc::{RPC_LIMITS_STORE_SETTING, RpcLimits};
use crate::sync::{NoteTagRecord, StateSyncUpdate};
use crate::transaction::{TransactionRecord, TransactionStoreUpdate};

mod account;
pub use account::{
    AccountRecord,
    AccountRecordData,
    AccountRecordError,
    AccountStatus,
    ClientAccountType,
};

mod chain_data;
pub use chain_data::BlockRelevance;

mod errors;
pub use errors::*;

mod filters;
pub use filters::{
    AccountStorageFilter,
    PartialBlockchainFilter,
    TransactionFilter,
    TransactionFilterQuery,
};

mod note_record;
pub use note_record::{
    InputNoteCursor,
    InputNoteRecord,
    InputNoteState,
    NoteExportType,
    NoteFilter,
    NoteRecordError,
    OutputNoteRecord,
    OutputNoteState,
    input_note_states,
};

mod note_update_tracker;
pub use note_update_tracker::{
    InputNoteUpdate,
    NoteUpdateTracker,
    NoteUpdateType,
    OutputNoteUpdate,
};

mod settings;
pub use settings::{SettingMutation, SettingScope, protocol_config_setting_key};

mod smt_forest;
pub use smt_forest::{AccountSmtForest, AccountUpdate};

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
/// mutability is expected to be implemented, which is why all methods receive `&self` and not `&mut
/// self`.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Store: Send + Sync {
    /// Returns an identifier for this store (e.g. `IndexedDB` database name, `SQLite` file path).
    ///
    /// This allows callers to retrieve store-specific identity information (such as the `IndexedDB`
    /// database name) for standalone operations like `exportStore`/`importStore`, without making
    /// import/export a responsibility of the client.
    fn identifier(&self) -> &str;

    /// Returns the current timestamp tracked by the store, measured in non-leap seconds since Unix
    /// epoch. If the store implementation is incapable of tracking time, it should return `None`.
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
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AccountNoteTagNotStorable`] if a new tag has a
    /// [`NoteTagSource::Account`](crate::sync::NoteTagSource::Account) source. The store applies no
    /// part of the update.
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

    /// Retrieves the input note following `cursor` in the filtered set for the given consumer
    /// account, or the first matching note when `cursor` is `None`. Optionally restricts to a block
    /// range via `block_start` and `block_end`. Returns `None` when no matching note follows the
    /// cursor.
    ///
    /// Build the cursor for the next call from the returned record with
    /// [`InputNoteCursor::from_record`].
    ///
    /// # Ordering
    ///
    /// Notes are sorted by their per-account on-chain execution order: block number, then
    /// per-account transaction order within the block. Notes consumed by the same transaction are
    /// ordered deterministically and consistently across calls.
    async fn get_input_note_after(
        &self,
        filter: NoteFilter,
        consumer: AccountId,
        block_start: Option<BlockNumber>,
        block_end: Option<BlockNumber>,
        cursor: Option<InputNoteCursor>,
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

    /// Returns the chain MMR peaks at the current sync height (peaks at `forest = block_num`, i.e.
    /// excluding `block_num` itself as a leaf).
    ///
    /// The peaks' `forest().num_leaves()` equals the current sync height by construction, so
    /// callers can derive the synced block number from the returned peaks without a second query.
    ///
    /// Before the first sync, returns an empty [`MmrPeaks`].
    async fn get_current_blockchain_peaks(&self) -> Result<MmrPeaks, StoreError>;

    /// Inserts a block header together with its MMR authentication nodes in a single transaction,
    /// so the header and the nodes that rebuild its `PartialMmr` are committed together.
    ///
    /// The header is inserted-if-not-exists with a one-way `has_client_notes` upgrade: on conflict
    /// the stored `header` is preserved and the flag only moves from `false` to `true`, never back.
    /// The MMR nodes are likewise inserted-if-not-exists: an `InOrderIndex` already present is left
    /// untouched (auth paths of tracked blocks share internal nodes, so re-inserting an existing
    /// index must be a no-op, not an error).
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
    /// Deletes all historical entries with `replaced_at_nonce <= up_to_nonce` from the historical
    /// tables (headers, storage, storage map entries, and assets).
    ///
    /// Also removes orphaned `account_code` entries that are no longer referenced by any account
    /// header.
    ///
    /// Returns the total number of rows deleted, including historical entries and orphaned account
    /// code.
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

    /// Retrieves the [`AccountCode`] for the specified account. Returns `None` if the account is
    /// not found.
    async fn get_account_code(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountCode>, StoreError>;

    /// Inserts an [`Account`] to the store, alongside its initial [`Address`].
    ///
    /// If the account is native, the address adds a tag to [`Self::get_account_note_tags`].
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
    /// If the account is native, the address adds a tag to [`Self::get_account_note_tags`].
    async fn insert_address(
        &self,
        address: Address,
        account_id: AccountId,
    ) -> Result<(), StoreError>;

    /// Removes an [`Address`]. Returns `true` if the address was tracked.
    ///
    /// The tag of the address stays in [`Self::get_account_note_tags`] while another address of the
    /// account has the same tag.
    async fn remove_address(&self, address: Address) -> Result<bool, StoreError>;

    // ACCOUNT WITNESSES
    // --------------------------------------------------------------------------------------------

    /// Registers an account whose [`AccountWitness`] should be refreshed on every sync, so that
    /// transactions using it as a foreign account can resolve the witness locally.
    ///
    /// No-op if the account is already registered; a cached witness is left in place. The witness
    /// itself is filled in by the next sync.
    ///
    /// Returns `true` if the account was not registered before this call.
    async fn track_account_witness(&self, account_id: AccountId) -> Result<bool, StoreError>;

    /// Stops refreshing the account's witness and drops any cached one.
    ///
    /// Returns `true` if the account was registered.
    async fn untrack_account_witness(&self, account_id: AccountId) -> Result<bool, StoreError>;

    /// Retrieves the ID of every registered account, whether or not a witness has been cached for
    /// it yet.
    async fn tracked_account_witnesses(&self) -> Result<Vec<AccountId>, StoreError>;

    /// Retrieves the cached [`AccountWitness`]. The witness opens under the account root of the
    /// block at the sync height.
    ///
    /// Returns `None` when the account is not registered or has not been refreshed yet.
    async fn get_account_witness(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountWitness>, StoreError>;

    /// Caches an [`AccountWitness`] for a registered account, replacing any previous one.
    ///
    /// Returns `false` if the account is not registered, in which case nothing is written.
    /// Registering is [`Self::track_account_witness`]'s job alone.
    ///
    /// The caller must verify the witness against the account root of the block at the sync height
    /// first. The read path does not check the witness, so a bad witness stored here surfaces later
    /// as a kernel assertion during execution rather than as a chain validation error at sync time.
    async fn update_account_witness(
        &self,
        account_id: AccountId,
        witness: &AccountWitness,
    ) -> Result<bool, StoreError>;

    // SETTINGS
    // --------------------------------------------------------------------------------------------

    /// Adds a value to `scope` in the `settings` table.
    async fn set_setting(
        &self,
        scope: SettingScope,
        key: String,
        value: Vec<u8>,
    ) -> Result<(), StoreError>;

    /// Retrieves a value from `scope` in the `settings` table.
    async fn get_setting(
        &self,
        scope: SettingScope,
        key: String,
    ) -> Result<Option<Vec<u8>>, StoreError>;

    /// Deletes a value from `scope` in the `settings` table. Returns `true` if the key was present.
    async fn remove_setting(&self, scope: SettingScope, key: String) -> Result<bool, StoreError>;

    /// Returns the keys held by `scope` in the `settings` table.
    async fn list_setting_keys(&self, scope: SettingScope) -> Result<Vec<String>, StoreError>;

    /// Applies a batch of [`SettingMutation`]s against `scope`. Use this when several `settings`
    /// entries must stay mutually consistent (e.g. a record and its secondary index).
    async fn apply_settings_mutations(
        &self,
        scope: SettingScope,
        mutations: Vec<SettingMutation>,
    ) -> Result<(), StoreError>;

    // SYNC
    // --------------------------------------------------------------------------------------------

    /// Returns the stored note tag records that the client is interested in.
    ///
    /// The result does not contain the records of [`Self::get_account_note_tags`].
    async fn get_note_tags(&self) -> Result<Vec<NoteTagRecord>, StoreError>;

    /// Returns the note tag records of the tracked native accounts.
    ///
    /// The store does not keep these records. This method derives them from the addresses of the
    /// native accounts, with [`Address::to_note_tag`]. Each record has a
    /// [`NoteTagSource::Account`](crate::sync::NoteTagSource::Account) source. Two addresses of one
    /// account with the same tag give one record. Watched accounts give no records.
    async fn get_account_note_tags(&self) -> Result<Vec<NoteTagRecord>, StoreError> {
        let mut tags = BTreeSet::new();
        for account_id in self.get_account_ids().await? {
            let is_native = self
                .get_minimal_partial_account(account_id)
                .await?
                .is_some_and(|record| !record.is_watched());
            if !is_native {
                continue;
            }
            for address in self.get_addresses_by_account_id(account_id).await? {
                tags.insert((address.to_note_tag(), account_id));
            }
        }

        Ok(tags
            .into_iter()
            .map(|(tag, account_id)| NoteTagRecord::with_account_source(tag, account_id))
            .collect())
    }

    /// Returns the unique note tags (without source) that the client is interested in.
    ///
    /// The result contains the tags of [`Self::get_note_tags`] and of
    /// [`Self::get_account_note_tags`].
    async fn get_unique_note_tags(&self) -> Result<BTreeSet<NoteTag>, StoreError> {
        let mut tags: BTreeSet<NoteTag> =
            self.get_note_tags().await?.into_iter().map(|r| r.tag).collect();
        tags.extend(self.get_account_note_tags().await?.into_iter().map(|r| r.tag));
        Ok(tags)
    }

    /// Adds a note tag to the list of tags that the client is interested in.
    ///
    /// If the tag was already being tracked, returns false since no new tags were actually added.
    /// Otherwise true.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AccountNoteTagNotStorable`] if the source of `tag` is
    /// [`NoteTagSource::Account`](crate::sync::NoteTagSource::Account). The store derives these
    /// records from the addresses of the native accounts.
    async fn add_note_tag(&self, tag: NoteTagRecord) -> Result<bool, StoreError>;

    /// Removes a note tag from the list of tags that the client is interested in.
    ///
    /// Returns the number of tags that were removed.
    async fn remove_note_tag(&self, tag: NoteTagRecord) -> Result<usize, StoreError>;

    /// Returns the block number of the last state sync block.
    async fn get_sync_height(&self) -> Result<BlockNumber, StoreError>;

    /// Applies the state sync update to the store. An update involves:
    ///
    /// - Inserting the new block header to the store alongside new MMR peaks information.
    /// - Updating the corresponding tracked input/output notes. Consumed notes carry consumption
    ///   metadata — `consumed_block_height`, `consumed_tx_order`, and `consumer_account_id` — in
    ///   their note state. Implementations must persist these fields so that ordered queries (see
    ///   [`Store::get_input_note_after`]) work correctly.
    /// - Removing note tags that are no longer relevant.
    /// - Updating transactions in the store, marking as `committed` or `discarded`.
    ///   - In turn, validating private account's state transitions. If a private account's
    ///     commitment locally does not match the `StateSyncUpdate` information, the account may be
    ///     locked.
    /// - Storing new MMR authentication nodes.
    /// - Updating the tracked public accounts.
    /// - Storing the protocol configuration the update carries, before the sync height advances.
    async fn apply_state_sync(&self, state_sync_update: StateSyncUpdate) -> Result<(), StoreError>;

    // TRANSPORT
    // --------------------------------------------------------------------------------------------

    /// Gets the unused aggregate note transport cursor.
    ///
    /// The client stores a cursor for each tag. If the aggregate cursor does not exist, this
    /// returns an initial cursor.
    #[deprecated(since = "0.17.1", note = "note transport stores a cursor for each tag")]
    #[allow(deprecated)]
    async fn get_note_transport_cursor(&self) -> Result<NoteTransportCursor, StoreError> {
        let Some(cursor_bytes) = self
            .get_setting(SettingScope::Client, NOTE_TRANSPORT_CURSOR_STORE_SETTING.into())
            .await?
        else {
            return Ok(NoteTransportCursor::init());
        };
        NoteTransportCursor::read_from_bytes(&cursor_bytes).map_err(Into::into)
    }

    /// Updates the unused aggregate note transport cursor.
    ///
    /// The client stores a cursor for each tag and does not read this value.
    #[deprecated(since = "0.17.1", note = "note transport stores a cursor for each tag")]
    #[allow(deprecated)]
    async fn update_note_transport_cursor(
        &self,
        cursor: NoteTransportCursor,
    ) -> Result<(), StoreError> {
        let cursor_bytes = cursor.to_bytes();
        self.set_setting(
            SettingScope::Client,
            NOTE_TRANSPORT_CURSOR_STORE_SETTING.into(),
            cursor_bytes,
        )
        .await?;
        Ok(())
    }

    // RPC LIMITS
    // --------------------------------------------------------------------------------------------

    /// Gets persisted RPC limits. Returns `None` if not stored.
    async fn get_rpc_limits(&self) -> Result<Option<RpcLimits>, StoreError> {
        let Some(bytes) =
            self.get_setting(SettingScope::Client, RPC_LIMITS_STORE_SETTING.into()).await?
        else {
            return Ok(None);
        };
        let limits = RpcLimits::read_from_bytes(&bytes)?;
        Ok(Some(limits))
    }

    /// Persists RPC limits to the store.
    async fn set_rpc_limits(&self, limits: RpcLimits) -> Result<(), StoreError> {
        self.set_setting(SettingScope::Client, RPC_LIMITS_STORE_SETTING.into(), limits.to_bytes())
            .await
    }

    // TRANSACTION ENCRYPTION KEY
    // --------------------------------------------------------------------------------------------

    /// Gets the cached transaction encryption key. Returns `None` if not stored.
    ///
    /// The key is public data shared by the whole validator set, so it is cached rather than
    /// treated as a secret.
    async fn get_transaction_encryption_key(
        &self,
    ) -> Result<Option<TransactionEncryptionKey>, StoreError> {
        let Some(bytes) = self
            .get_setting(SettingScope::Client, TRANSACTION_ENCRYPTION_KEY_STORE_SETTING.into())
            .await?
        else {
            return Ok(None);
        };
        let key = TransactionEncryptionKey::read_from_bytes(&bytes)?;
        Ok(Some(key))
    }

    /// Caches the transaction encryption key, replacing any previously cached key.
    async fn set_transaction_encryption_key(
        &self,
        key: &TransactionEncryptionKey,
    ) -> Result<(), StoreError> {
        self.set_setting(
            SettingScope::Client,
            TRANSACTION_ENCRYPTION_KEY_STORE_SETTING.into(),
            key.to_bytes(),
        )
        .await
    }

    /// Removes the cached transaction encryption key, so the next submission fetches and verifies a
    /// fresh one. Used when the node rejects a submission sealed against a retired key.
    async fn remove_transaction_encryption_key(&self) -> Result<(), StoreError> {
        self.remove_setting(SettingScope::Client, TRANSACTION_ENCRYPTION_KEY_STORE_SETTING.into())
            .await?;
        Ok(())
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

    /// Retrieves all assets in the account's vault as a plain list, without building the vault's
    /// Merkle tree.
    ///
    /// Prefer this over [`Store::get_account_vault`] when only asset values are needed (e.g.
    /// balance checks): it avoids hashing every asset into an SMT.
    ///
    /// The default implementation of this method uses [`Store::get_account_vault`].
    async fn get_account_assets(&self, account_id: AccountId) -> Result<Vec<Asset>, StoreError> {
        Ok(self.get_account_vault(account_id).await?.assets().collect())
    }

    /// Returns vault asset witnesses for `asset_ids` against the account's vault with root
    /// `vault_root`. An asset absent from the vault yields an emptiness proof rather than an error,
    /// which the executor needs when an asset is being added to the vault.
    ///
    /// The default implementation reconstructs the vault via [`Store::get_account_vault`] and opens
    /// each witness from it; backends that keep an in-memory Merkle forest (e.g. `SqliteStore`)
    /// override it to open the witnesses directly, without materializing the vault.
    async fn get_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        asset_ids: BTreeSet<AssetId>,
    ) -> Result<Vec<AssetWitness>, StoreError> {
        let vault = self.get_account_vault(account_id).await?;
        if vault.root() != vault_root {
            return Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                expected_root: vault_root,
                actual_root: vault.root(),
            }));
        }
        Ok(asset_ids.into_iter().map(|asset_id| vault.open(asset_id)).collect())
    }

    /// Retrieves a specific asset (by vault id) from the account's vault along with its Merkle
    /// witness.
    ///
    /// The default implementation of this method uses [`Store::get_account_vault`].
    async fn get_account_asset(
        &self,
        account_id: AccountId,
        asset_id: AssetId,
    ) -> Result<Option<(Asset, AssetWitness)>, StoreError> {
        let vault = self.get_account_vault(account_id).await?;
        let Some(asset) = vault.assets().find(|a| a.id() == asset_id) else {
            return Ok(None);
        };

        let witness = vault.open(asset_id);

        Ok(Some((asset, witness)))
    }

    /// Retrieves the storage for a specific account.
    ///
    /// Can take an optional map root to retrieve only part of the storage, If it does, it will
    /// either return an account storage with a single slot (the one requested), or an error if not
    /// found.
    async fn get_account_storage(
        &self,
        account_id: AccountId,
        filter: AccountStorageFilter,
    ) -> Result<AccountStorage, StoreError>;

    /// Retrieves a storage slot value by name.
    ///
    /// For `Value` slots, returns the stored word. For `Map` slots, returns the map root.
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

    // IN-BATCH (STAGED) WITNESSES
    // --------------------------------------------------------------------------------------------

    // PARTIAL ACCOUNTS
    // --------------------------------------------------------------------------------------------

    /// Retrieves an [`AccountRecord`] object, this contains the account's latest partial state
    /// along with its status. Returns `None` if the partial account is not found.
    async fn get_minimal_partial_account(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountRecord>, StoreError>;
}

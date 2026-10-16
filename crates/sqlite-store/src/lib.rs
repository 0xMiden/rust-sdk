//! SQLite-backed Store implementation for miden-client. This crate provides `SqliteStore` and its
//! full implementation.
//!
//! [`SqliteStore`] enables the persistence of accounts, transactions, notes, block headers, and MMR
//! nodes using an `SQLite` database.

use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, TryLockError};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::string::{String, ToString};
use std::time::Duration;
use std::vec::Vec;

use db_management::migration::SqliteMigrator;
use db_management::pool_manager::{Pool, SqlitePoolManager};
use deadpool::Runtime;
use miden_client_core::errors::StoreError;
use miden_client_core::store::{
    AccountRecord,
    AccountStatus,
    AccountStorageFilter,
    BlockRelevance,
    ClientAccountType,
    InputNoteCursor,
    InputNoteRecord,
    NoteFilter,
    OutputNoteRecord,
    PartialBlockchainFilter,
    SettingMutation,
    SettingScope,
    Store,
    TransactionFilter,
};
use miden_client_core::sync::{NoteTagRecord, StateSyncUpdate};
use miden_client_core::transaction::{TransactionRecord, TransactionStoreUpdate};
use miden_client_proto as proto;
use miden_protocol::account::{
    Account,
    AccountCode,
    AccountHeader,
    AccountId,
    AccountStorage,
    StorageMapKey,
    StorageMapWitness,
    StorageSlotName,
};
use miden_protocol::address::Address;
use miden_protocol::asset::{Asset, AssetId, AssetVault, AssetWitness};
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::{InOrderIndex, MmrPeaks};
use miden_protocol::note::{NoteScript, Nullifier};
use miden_protocol::utils::serde::Serializable;
use miden_protocol::{Felt, Word};
use rusqlite::Connection;
use rusqlite::types::Value;
use sql_error::SqlResultExt;

use crate::account::rows::query_vault_assets;

mod account;
mod chain_data;
mod db_management;
mod forest;
mod note;
mod settings;
mod sql_error;
mod sync;
mod transaction;

// SQLITE STORE
// ================================================================================================

/// `SQLite`-backed [`Store`] implementation.
///
/// Current table definitions are the result of applying every migration under `migrations/` in
/// order.
///
/// # Concurrent access
///
/// Only one client instance must use a database file at a time. Two clients that sync, import
/// notes, or apply transactions on the same file at the same time can leave the store in an
/// incorrect state, because each operation reads the store, waits for the node, and then writes.
/// `SQLite` makes each write atomic, but it cannot prevent a write that is based on data that
/// another client changed.
///
/// To prevent this, each store locks a `.lock` file next to the database for its lifetime:
///
/// - [`SqliteStore::new`] takes an exclusive lock. No other store can open the file while this store is alive.
/// - [`SqliteStore::new_unlocked`] takes a shared lock. Other unlocked stores can open the file, but a store from [`SqliteStore::new`] cannot.
///
/// A store that cannot get its lock fails to open with [`StoreError::StoreLocked`]. The store does
/// not wait for the lock.
///
/// The lock applies to stores in the same process and in different processes. It is advisory, so it
/// does not stop a program that opens the database file without a `SqliteStore`.
///
/// The lock does not stop several clients from sharing one `SqliteStore` instance, for example
/// through clones of an `Arc<SqliteStore>`. Do not share one store between clients.
///
/// The operating system releases the lock when the store is dropped or the process stops, also
/// after a crash or a restart. The `.lock` file stays on disk and has no effect when no store holds
/// it. Do not delete it while a store is open. On network file systems, the lock can be unreliable
/// or can stay held for some time after a crash.
pub struct SqliteStore {
    pub(crate) pool: Pool,
    database_filepath: PathBuf,
    /// The open `.lock` file that holds the lock on the database file.
    ///
    /// Closing this handle releases the lock. This field is declared after `pool`, so all
    /// connections close before the lock is released.
    _lock: File,
}

impl SqliteStore {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    /// Opens the store at `database_filepath` and holds an exclusive lock on it until the store is
    /// dropped.
    ///
    /// Creates the database if it does not exist and applies the schema migrations. The lock is
    /// taken before the migrations run. See [Concurrent access](SqliteStore#concurrent-access) for
    /// how the lock works.
    ///
    /// Use this constructor unless several stores must open the same file.
    ///
    /// # Errors
    ///
    /// - [`StoreError::StoreLocked`] if another store holds a lock on the file, in this process or in a different process.
    /// - [`StoreError::DatabaseError`] if the path is not valid UTF-8, if the `.lock` file cannot be opened or locked, or if the database cannot be opened or migrated.
    pub async fn new(database_filepath: PathBuf) -> Result<Self, StoreError> {
        Self::open(database_filepath, true).await
    }

    /// Opens the store at `database_filepath` and holds a shared lock on it until the store is
    /// dropped.
    ///
    /// Other stores from this constructor can open the same file at the same time. A store from
    /// [`SqliteStore::new`] cannot. See [Concurrent access](SqliteStore#concurrent-access) for how
    /// the lock works.
    ///
    /// The store does not coordinate with the other unlocked stores on the file. The caller must
    /// make sure that only one of them syncs, imports notes, or applies transactions at a time. Use
    /// this constructor only for tools and tests that need several stores on one file, for example
    /// to read the store while a client is open.
    ///
    /// # Errors
    ///
    /// - [`StoreError::StoreLocked`] if a store from [`SqliteStore::new`] holds the file.
    /// - [`StoreError::DatabaseError`] if the path is not valid UTF-8, if the `.lock` file cannot be opened or locked, or if the database cannot be opened or migrated.
    pub async fn new_unlocked(database_filepath: PathBuf) -> Result<Self, StoreError> {
        Self::open(database_filepath, false).await
    }

    /// Opens the store and takes an exclusive lock if `exclusive` is `true`, or a shared lock
    /// otherwise.
    async fn open(database_filepath: PathBuf, exclusive: bool) -> Result<Self, StoreError> {
        if database_filepath.to_str().is_none() {
            return Err(database_error(format!(
                "database path is not valid UTF-8: {}",
                database_filepath.display()
            )));
        }

        // The lock is taken before the migrations, so a locked store never migrates a file that
        // another store uses.
        let lock = lock_store(&database_filepath, exclusive)?;

        let sqlite_pool_manager = SqlitePoolManager::new(database_filepath.clone());
        let pool = Pool::builder(sqlite_pool_manager)
            .wait_timeout(Some(Duration::from_secs(30)))
            .runtime(Runtime::Tokio1)
            .build()
            .map_err(database_error)?;

        Self::migrate(&pool, SqliteMigrator::client()).await?;

        // Account SMT data is persisted in the forest tables and read on demand, so no state needs
        // to be rebuilt here.
        Ok(SqliteStore { pool, database_filepath, _lock: lock })
    }

    /// Returns the path of the database file backing this store.
    pub fn database_filepath(&self) -> &Path {
        &self.database_filepath
    }

    /// Brings the database in `pool` up to the latest version of the schema `migration` builds.
    ///
    /// The upgrade is verified before it is committed, so a failure is rolled back by `SQLite` and
    /// leaves the store exactly as it was.
    async fn migrate(pool: &Pool, migration: &'static SqliteMigrator) -> Result<(), StoreError> {
        let conn = pool.get().await.map_err(database_error)?;

        conn.interact(move |conn| migration.apply(conn))
            .await
            .map_err(database_error)?
            .map_err(database_error)
    }

    /// Interacts with the database by executing the provided function on a connection from the
    /// pool.
    ///
    /// This function is a helper method which simplifies the process of making queries to the
    /// database. It acquires a connection from the pool and executes the provided function,
    /// returning the result.
    async fn interact_with_connection<F, R>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Connection) -> Result<R, StoreError> + Send + 'static,
        R: Send + 'static,
    {
        self.pool
            .get()
            .await
            .map_err(database_error)?
            .interact(f)
            .await
            .map_err(database_error)?
    }
}

// SQLite implementation of the Store trait
//
// To simplify, all implementations rely on inner SqliteStore functions that map 1:1 by name This
// way, the actual implementations are grouped by entity types in their own sub-modules
#[async_trait::async_trait]
impl Store for SqliteStore {
    fn identifier(&self) -> &str {
        self.database_filepath
            .to_str()
            .expect("rejected by SqliteStore::new when not UTF-8")
    }

    fn get_current_timestamp(&self) -> Option<u64> {
        Some(current_timestamp_u64())
    }

    async fn get_note_tags(&self) -> Result<Vec<NoteTagRecord>, StoreError> {
        self.interact_with_connection(SqliteStore::get_note_tags).await
    }

    async fn add_note_tag(&self, tag: NoteTagRecord) -> Result<bool, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::add_note_tag(conn, tag))
            .await
    }

    async fn remove_note_tag(&self, tag: NoteTagRecord) -> Result<usize, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::remove_note_tag(conn, tag))
            .await
    }

    async fn get_sync_height(&self) -> Result<BlockNumber, StoreError> {
        self.interact_with_connection(SqliteStore::get_sync_height).await
    }

    async fn apply_state_sync(&self, state_sync_update: StateSyncUpdate) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::apply_state_sync(conn, state_sync_update)
        })
        .await
    }

    async fn get_transactions(
        &self,
        transaction_filter: TransactionFilter,
    ) -> Result<Vec<TransactionRecord>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_transactions(conn, &transaction_filter)
        })
        .await
    }

    async fn apply_transaction(&self, tx_update: TransactionStoreUpdate) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::apply_transaction(conn, &tx_update))
            .await
    }

    async fn apply_transaction_batch(
        &self,
        tx_updates: Vec<TransactionStoreUpdate>,
    ) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::apply_transaction_batch(conn, &tx_updates)
        })
        .await
    }

    async fn get_input_notes(
        &self,
        filter: NoteFilter,
    ) -> Result<Vec<InputNoteRecord>, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::get_input_notes(conn, &filter))
            .await
    }

    async fn get_output_notes(
        &self,
        note_filter: NoteFilter,
    ) -> Result<Vec<OutputNoteRecord>, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::get_output_notes(conn, &note_filter))
            .await
    }

    async fn get_input_note_after(
        &self,
        filter: NoteFilter,
        consumer: AccountId,
        block_start: Option<BlockNumber>,
        block_end: Option<BlockNumber>,
        cursor: Option<InputNoteCursor>,
    ) -> Result<Option<InputNoteRecord>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_input_note_after(
                conn,
                &filter,
                consumer,
                block_start,
                block_end,
                cursor,
            )
        })
        .await
    }

    async fn upsert_input_notes(&self, notes: &[InputNoteRecord]) -> Result<(), StoreError> {
        let notes = notes.to_vec();
        self.interact_with_connection(move |conn| SqliteStore::upsert_input_notes(conn, &notes))
            .await
    }

    async fn get_note_script(&self, script_root: Word) -> Result<NoteScript, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::get_note_script(conn, script_root))
            .await
    }

    async fn upsert_note_scripts(&self, note_scripts: &[NoteScript]) -> Result<(), StoreError> {
        let note_scripts = note_scripts.to_vec();
        self.interact_with_connection(move |conn| {
            SqliteStore::upsert_note_scripts(conn, &note_scripts)
        })
        .await
    }

    async fn insert_block_header(
        &self,
        block_header: &BlockHeader,
        nodes: &[(InOrderIndex, Word)],
        has_client_notes: bool,
    ) -> Result<(), StoreError> {
        let block_header = block_header.clone();
        let nodes = nodes.to_vec();
        self.interact_with_connection(move |conn| {
            SqliteStore::insert_block_header(conn, &block_header, &nodes, has_client_notes)
        })
        .await
    }

    async fn untrack_and_prune_irrelevant_blocks(
        &self,
        blocks_to_untrack: &[BlockNumber],
        node_indices_to_remove: &[InOrderIndex],
    ) -> Result<(), StoreError> {
        let blocks_to_untrack = blocks_to_untrack.to_vec();
        let node_indices_to_remove = node_indices_to_remove.to_vec();
        self.interact_with_connection(move |conn| {
            SqliteStore::untrack_and_prune_irrelevant_blocks(
                conn,
                &blocks_to_untrack,
                &node_indices_to_remove,
            )
        })
        .await
    }

    async fn prune_account_history(
        &self,
        account_id: AccountId,
        up_to_nonce: Felt,
    ) -> Result<usize, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::prune_account_history(conn, account_id, up_to_nonce)
        })
        .await
    }

    async fn get_block_headers(
        &self,
        block_numbers: &BTreeSet<BlockNumber>,
    ) -> Result<Vec<(BlockHeader, BlockRelevance)>, StoreError> {
        let block_numbers = block_numbers.clone();
        self.interact_with_connection(move |conn| {
            SqliteStore::get_block_headers(conn, &block_numbers)
        })
        .await
    }

    async fn get_tracked_block_headers(&self) -> Result<Vec<BlockHeader>, StoreError> {
        self.interact_with_connection(SqliteStore::get_tracked_block_headers).await
    }

    async fn get_tracked_block_header_numbers(&self) -> Result<BTreeSet<usize>, StoreError> {
        self.interact_with_connection(SqliteStore::get_tracked_block_header_numbers)
            .await
    }

    async fn get_partial_blockchain_nodes(
        &self,
        filter: PartialBlockchainFilter,
    ) -> Result<BTreeMap<InOrderIndex, Word>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_partial_blockchain_nodes(conn, &filter)
        })
        .await
    }

    async fn get_current_blockchain_peaks(&self) -> Result<MmrPeaks, StoreError> {
        self.interact_with_connection(SqliteStore::get_current_blockchain_peaks).await
    }

    async fn insert_account(
        &self,
        account: &Account,
        initial_address: Address,
        client_account_type: ClientAccountType,
    ) -> Result<(), StoreError> {
        let cloned_account = account.clone();

        self.interact_with_connection(move |conn| {
            SqliteStore::insert_account(
                conn,
                &cloned_account,
                &initial_address,
                client_account_type,
            )
        })
        .await
    }

    async fn update_account(&self, account: &Account) -> Result<(), StoreError> {
        let cloned_account = account.clone();

        self.interact_with_connection(move |conn| {
            SqliteStore::update_account(conn, &cloned_account)
        })
        .await
    }

    async fn get_account_ids(&self) -> Result<Vec<AccountId>, StoreError> {
        self.interact_with_connection(SqliteStore::get_account_ids).await
    }

    async fn get_account_headers(&self) -> Result<Vec<(AccountHeader, AccountStatus)>, StoreError> {
        self.interact_with_connection(SqliteStore::get_account_headers).await
    }

    async fn get_account_header(
        &self,
        account_id: AccountId,
    ) -> Result<Option<(AccountHeader, AccountStatus)>, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::get_account_header(conn, account_id))
            .await
    }

    async fn get_account_header_by_commitment(
        &self,
        account_commitment: Word,
    ) -> Result<Option<AccountHeader>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_header_by_commitment(conn, account_commitment)
        })
        .await
    }

    async fn get_account(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountRecord>, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::get_account(conn, account_id))
            .await
    }

    async fn get_account_code(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountCode>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_code_by_id(conn, account_id)
        })
        .await
    }

    async fn upsert_foreign_account_code(
        &self,
        account_id: AccountId,
        code: AccountCode,
    ) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::upsert_foreign_account_code(conn, account_id, &code)
        })
        .await
    }

    async fn get_foreign_account_code(
        &self,
        account_ids: Vec<AccountId>,
    ) -> Result<BTreeMap<AccountId, AccountCode>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_foreign_account_code(conn, account_ids)
        })
        .await
    }

    async fn track_account_witness(&self, account_id: AccountId) -> Result<bool, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::track_account_witness(conn, account_id)
        })
        .await
    }

    async fn untrack_account_witness(&self, account_id: AccountId) -> Result<bool, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::untrack_account_witness(conn, account_id)
        })
        .await
    }

    async fn tracked_account_witnesses(&self) -> Result<Vec<AccountId>, StoreError> {
        self.interact_with_connection(SqliteStore::tracked_account_witnesses).await
    }

    async fn get_account_witness(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountWitness>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_witness(conn, account_id)
        })
        .await
    }

    async fn update_account_witness(
        &self,
        account_id: AccountId,
        witness: &AccountWitness,
    ) -> Result<bool, StoreError> {
        let witness = witness.clone();
        self.interact_with_connection(move |conn| {
            SqliteStore::update_account_witness(conn, account_id, &witness)
        })
        .await
    }

    async fn set_setting(
        &self,
        scope: SettingScope,
        key: String,
        value: Vec<u8>,
    ) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::set_setting(conn, scope, &key, &value)
        })
        .await
    }

    async fn get_setting(
        &self,
        scope: SettingScope,
        key: String,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::get_setting(conn, scope, &key))
            .await
    }

    async fn remove_setting(&self, scope: SettingScope, key: String) -> Result<bool, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::remove_setting(conn, scope, &key))
            .await
    }

    async fn list_setting_keys(&self, scope: SettingScope) -> Result<Vec<String>, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::list_setting_keys(conn, scope))
            .await
    }

    async fn apply_settings_mutations(
        &self,
        scope: SettingScope,
        mutations: Vec<SettingMutation>,
    ) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            with_write_tx(conn, |tx| {
                for mutation in &mutations {
                    match mutation {
                        SettingMutation::Set { key, value } => {
                            SqliteStore::set_setting(tx, scope, key, value)?;
                        },
                        SettingMutation::Remove { key } => {
                            SqliteStore::remove_setting(tx, scope, key)?;
                        },
                    }
                }
                Ok(())
            })
        })
        .await
    }

    async fn get_unspent_input_note_nullifiers(&self) -> Result<Vec<Nullifier>, StoreError> {
        self.interact_with_connection(SqliteStore::get_unspent_input_note_nullifiers)
            .await
    }

    async fn get_account_vault(&self, account_id: AccountId) -> Result<AssetVault, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::get_account_vault(conn, account_id))
            .await
    }

    async fn get_account_assets(&self, account_id: AccountId) -> Result<Vec<Asset>, StoreError> {
        self.interact_with_connection(move |conn| query_vault_assets(conn, account_id))
            .await
    }

    async fn get_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        asset_ids: BTreeSet<AssetId>,
    ) -> Result<Vec<AssetWitness>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_vault_asset_witnesses(conn, account_id, vault_root, asset_ids)
        })
        .await
    }

    async fn get_account_asset(
        &self,
        account_id: AccountId,
        asset_id: AssetId,
    ) -> Result<Option<(Asset, AssetWitness)>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_asset(conn, account_id, asset_id)
        })
        .await
    }

    async fn get_account_map_item(
        &self,
        account_id: AccountId,
        slot_name: StorageSlotName,
        key: StorageMapKey,
    ) -> Result<(Word, StorageMapWitness), StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_map_item(conn, account_id, slot_name, key)
        })
        .await
    }

    async fn get_account_storage(
        &self,
        account_id: AccountId,
        filter: AccountStorageFilter,
    ) -> Result<AccountStorage, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_storage(conn, account_id, &filter)
        })
        .await
    }

    async fn get_addresses_by_account_id(
        &self,
        account_id: AccountId,
    ) -> Result<Vec<Address>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_addresses(conn, account_id)
        })
        .await
    }

    async fn insert_address(
        &self,
        address: Address,
        account_id: AccountId,
    ) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::insert_address(conn, &address, account_id)
        })
        .await
    }

    async fn remove_address(&self, address: Address) -> Result<bool, StoreError> {
        self.interact_with_connection(move |conn| SqliteStore::remove_address(conn, &address))
            .await
    }

    async fn get_minimal_partial_account(
        &self,
        account_id: AccountId,
    ) -> Result<Option<AccountRecord>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_minimal_partial_account(conn, account_id)
        })
        .await
    }
}

// UTILS
// ================================================================================================

/// Locks the `.lock` file next to the database and returns the handle that holds the lock.
///
/// The lock is on a separate file because `SQLite` takes its own locks on the database file and its
/// journal files. An exclusive lock conflicts with every other lock on the file. A shared lock
/// conflicts only with an exclusive one. The function does not wait. It returns
/// [`StoreError::StoreLocked`] if the lock conflicts with a lock that another handle holds.
///
/// The operating system releases the lock when the handle closes or the process stops, so a crash
/// cannot leave the store locked. The file is not deleted. A deleted file lets a new store lock a
/// new file while an old store still holds the lock on the deleted one.
fn lock_store(database_filepath: &Path, exclusive: bool) -> Result<File, StoreError> {
    let mut lock_path = database_filepath.as_os_str().to_owned();
    lock_path.push(".lock");

    let file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(database_error)?;
    let locked = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    locked.map_err(|err| match err {
        TryLockError::WouldBlock => {
            StoreError::StoreLocked(database_filepath.display().to_string())
        },
        TryLockError::Error(err) => database_error(err),
    })?;

    Ok(file)
}

fn database_error(err: impl core::fmt::Display) -> StoreError {
    StoreError::DatabaseError(err.to_string())
}

/// Returns the current UTC timestamp as `u64` (non-leap seconds since Unix epoch).
pub(crate) fn current_timestamp_u64() -> u64 {
    let now = chrono::Utc::now();
    u64::try_from(now.timestamp()).expect("timestamp is always after epoch")
}

/// Gets a `u64` value from the database.
///
/// `Sqlite` uses `i64` as its internal representation format, and so when retrieving we need to
/// make sure we cast as `u64` to get the original value
pub(crate) fn column_value_as_u64<I: rusqlite::RowIndex>(
    row: &rusqlite::Row<'_>,
    index: I,
) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    #[allow(
        clippy::cast_sign_loss,
        reason = "We store u64 as i64 as sqlite only allows the latter."
    )]
    Ok(value as u64)
}

/// Converts a `u64` into a [Value].
///
/// `Sqlite` uses `i64` as its internal representation format. Note that the `as` operator performs
/// a lossless conversion from `u64` to `i64`.
pub(crate) fn u64_to_value(v: u64) -> Value {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "We store u64 as i64 as sqlite only allows the latter."
    )]
    Value::Integer(v as i64)
}

/// Builds the value list for a `rarray(?)` parameter from serializable items, each stored as a BLOB
/// of its canonical byte encoding.
///
/// Binding the list as a single table-valued parameter keeps the SQL text constant, so the prepared
/// statement stays cacheable regardless of the list length (and the list is not subject to
/// `SQLite`'s bound-parameter limit).
pub(crate) fn blob_array<T: Serializable>(items: impl IntoIterator<Item = T>) -> Rc<Vec<Value>> {
    Rc::new(items.into_iter().map(|item| Value::Blob(item.to_bytes())).collect())
}

/// Builds the value list for a `rarray(?)` parameter from `u64` values, stored as SQL INTEGERs
/// through the same bit-cast as [`u64_to_value`].
pub(crate) fn int_array(items: impl IntoIterator<Item = u64>) -> Rc<Vec<Value>> {
    Rc::new(items.into_iter().map(u64_to_value).collect())
}

/// Builds the value list for a `rarray(?)` parameter from string values, stored as SQL TEXT.
pub(crate) fn text_array(items: impl IntoIterator<Item = String>) -> Rc<Vec<Value>> {
    Rc::new(items.into_iter().map(Value::Text).collect())
}

/// Runs `f` inside an `IMMEDIATE` rusqlite transaction. Commits on `Ok`, rolls back on `Err`.
///
/// The closure must write. An `IMMEDIATE` transaction takes the write lock at `BEGIN`, so a closure
/// that reads and then writes cannot lose the lock upgrade in between. In WAL mode that upgrade
/// fails with `SQLITE_BUSY_SNAPSHOT`, which the busy timeout does not retry.
pub(crate) fn with_write_tx<R>(
    conn: &mut Connection,
    f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<R, StoreError>,
) -> Result<R, StoreError> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .into_store_error()?;
    let result = f(&tx)?;
    tx.commit().into_store_error()?;
    Ok(result)
}

// TESTS
// ================================================================================================

#[cfg(test)]
pub mod tests {
    use std::boxed::Box;
    use std::sync::LazyLock;

    use miden_client::testing::common::create_test_store_path;
    use miden_client_core::store::Store;

    use super::db_management::migration::SqliteMigrator;
    use super::db_management::migration::tests::damaging_migration;
    use super::db_management::pool_manager::SqlitePoolManager;
    use super::{Pool, SqliteStore, StoreError, column_value_as_u64, u64_to_value, with_write_tx};

    /// A migration set that changes the store and is then rejected, which is the failure the
    /// rollback has to undo.
    static DAMAGING_MIGRATION: LazyLock<SqliteMigrator> = LazyLock::new(damaging_migration);

    #[tokio::test]
    async fn failed_migration_leaves_the_store_as_it_was() {
        let database_filepath = create_test_store_path();
        drop(SqliteStore::new(database_filepath.clone()).await.unwrap());

        let pool = Pool::builder(SqlitePoolManager::new(database_filepath.clone()))
            .build()
            .unwrap();
        let err = SqliteStore::migrate(&pool, &DAMAGING_MIGRATION).await.unwrap_err();

        assert!(
            err.to_string().contains("produced a schema this client does not expect"),
            "the migration should have been rejected, got {err}"
        );
        // Reopening verifies the schema, so it only succeeds if the dropped table is still there.
        SqliteStore::new(database_filepath).await.unwrap();
    }

    #[tokio::test]
    async fn store_lock_keeps_a_locked_store_alone() -> anyhow::Result<()> {
        let path = create_test_store_path();
        let locked = SqliteStore::new(path.clone()).await?;
        assert!(matches!(SqliteStore::new(path.clone()).await, Err(StoreError::StoreLocked(_))));
        assert!(matches!(
            SqliteStore::new_unlocked(path.clone()).await,
            Err(StoreError::StoreLocked(_))
        ));
        drop(locked);

        let unlocked = SqliteStore::new_unlocked(path.clone()).await?;
        SqliteStore::new_unlocked(path.clone()).await?;
        assert!(matches!(SqliteStore::new(path.clone()).await, Err(StoreError::StoreLocked(_))));
        drop(unlocked);

        SqliteStore::new(path).await?;

        Ok(())
    }

    fn assert_send_sync<T: Send + Sync>() {}

    /// The write path bit-casts `u64` to `i64` and the read path must bit-cast it back, including
    /// for values whose top bit is set (which are stored as negative SQL INTEGERs).
    #[test]
    fn u64_column_round_trip() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        for value in [0u64, 1, 1 << 63, u64::MAX] {
            let read: u64 = conn
                .query_row("SELECT ?1", [u64_to_value(value)], |row| column_value_as_u64(row, 0))
                .unwrap();
            assert_eq!(read, value);
        }
    }

    #[test]
    fn with_write_tx_rolls_back_on_error() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);").unwrap();

        let result = with_write_tx(&mut conn, |tx| {
            tx.execute("INSERT INTO t (id) VALUES (1)", []).unwrap();
            Err::<(), _>(StoreError::DatabaseError("forced failure".into()))
        });
        assert!(result.is_err());

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 0, "the insert must roll back when the closure errors");
    }

    #[test]
    fn with_write_tx_commits_on_success() {
        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY);").unwrap();

        with_write_tx(&mut conn, |tx| {
            tx.execute("INSERT INTO t (id) VALUES (1)", []).unwrap();
            Ok(())
        })
        .unwrap();

        let count: i64 = conn.query_row("SELECT COUNT(*) FROM t", [], |row| row.get(0)).unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn is_send_sync() {
        assert_send_sync::<SqliteStore>();
        assert_send_sync::<Box<dyn Store>>();
    }

    // Function that returns a `Send` future from a dynamic trait that must be `Sync`.
    async fn dyn_trait_send_fut(store: Box<dyn Store>) {
        // This wouldn't compile if `get_tracked_block_headers` doesn't return a `Send` future.
        let res = store.get_tracked_block_headers().await;
        assert!(res.is_ok());
    }

    #[tokio::test]
    async fn future_is_send() {
        let client = SqliteStore::new(create_test_store_path()).await.unwrap();
        let client: Box<SqliteStore> = client.into();
        tokio::task::spawn(async move { dyn_trait_send_fut(client).await });
    }

    pub(crate) async fn create_test_store() -> SqliteStore {
        SqliteStore::new(create_test_store_path()).await.unwrap()
    }
}

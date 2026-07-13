//! SQLite-backed Store implementation for miden-client.
//! This crate provides `SqliteStore` and its full implementation.
//!
//! [`SqliteStore`] enables the persistence of accounts, transactions, notes, block headers, and MMR
//! nodes using an `SQLite` database.

use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::string::{String, ToString};
use std::sync::{Arc, RwLock};
use std::vec::Vec;

use db_management::pool_manager::{Pool, SqlitePoolManager};
use db_management::utils::{
    apply_migrations,
    get_setting,
    list_setting_keys,
    remove_setting,
    set_setting,
};
use miden_client::Word;
use miden_client::account::{
    Account,
    AccountCode,
    AccountHeader,
    AccountId,
    AccountStorage,
    Address,
    StorageMapKey,
    StorageSlotName,
};
use miden_client::asset::{Asset, AssetVault, AssetWitness};
use miden_client::block::BlockHeader;
use miden_client::crypto::{InOrderIndex, MmrPeaks};
use miden_client::note::{BlockNumber, NoteScript, NoteTag, Nullifier};
use miden_client::store::{
    AccountRecord,
    AccountSmtForest,
    AccountStatus,
    AccountStorageFilter,
    BlockRelevance,
    ClientAccountType,
    InputNoteRecord,
    NoteFilter,
    OutputNoteRecord,
    PartialBlockchainFilter,
    SettingMutation,
    Store,
    StoreError,
    TransactionFilter,
};
use miden_client::sync::{NoteTagRecord, StateSyncUpdate};
use miden_client::transaction::{TransactionRecord, TransactionStoreUpdate};
use miden_protocol::Felt;
use miden_protocol::account::StorageMapWitness;
use miden_protocol::asset::AssetVaultKey;
use rusqlite::Connection;
use rusqlite::types::Value;
use sql_error::SqlResultExt;

mod account;
mod builder;
mod chain_data;
mod db_management;
mod note;
mod sql_error;
mod sync;
mod transaction;

pub use builder::ClientBuilderSqliteExt;

// SQLITE STORE
// ================================================================================================

/// Represents a pool of connections with an `SQLite` database. The pool is used to interact
/// concurrently with the underlying database in a safe and efficient manner.
///
/// Current table definitions can be found at `store.sql` migration file.
pub struct SqliteStore {
    pub(crate) pool: Pool,
    database_filepath: String,
    smt_forest: Arc<RwLock<AccountSmtForest>>,
}

impl SqliteStore {
    // CONSTRUCTORS
    // --------------------------------------------------------------------------------------------

    /// Returns a new instance of [Store] instantiated with the specified configuration options.
    pub async fn new(database_filepath: PathBuf) -> Result<Self, StoreError> {
        let database_filepath_str = database_filepath.to_string_lossy().into_owned();
        let sqlite_pool_manager = SqlitePoolManager::new(database_filepath);
        let pool = Pool::builder(sqlite_pool_manager)
            .build()
            .map_err(|e| StoreError::DatabaseError(e.to_string()))?;

        let conn = pool.get().await.map_err(|e| StoreError::DatabaseError(e.to_string()))?;

        conn.interact(apply_migrations)
            .await
            .map_err(|e| StoreError::DatabaseError(e.to_string()))?
            .map_err(|e| StoreError::DatabaseError(e.to_string()))?;

        let store = SqliteStore {
            pool,
            database_filepath: database_filepath_str,
            smt_forest: Arc::new(RwLock::new(AccountSmtForest::new())),
        };

        // Initialize SMT forest
        for id in store.get_account_ids().await? {
            let vault = store.get_account_vault(id).await?;
            let storage = store.get_account_storage(id, AccountStorageFilter::All).await?;
            let header = store.get_account_header(id).await?;

            let mut smt_forest = store.smt_forest.write().expect("smt write lock not poisoned");
            if header.is_some() {
                smt_forest.insert_and_register_account_state(id, &vault, &storage)?;
            } else {
                smt_forest.insert_account_state(&vault, &storage)?;
            }
        }

        Ok(store)
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
            .map_err(|err| StoreError::DatabaseError(err.to_string()))?
            .interact(f)
            .await
            .map_err(|err| StoreError::DatabaseError(err.to_string()))?
    }
}

// SQLite implementation of the Store trait
//
// To simplify, all implementations rely on inner SqliteStore functions that map 1:1 by name
// This way, the actual implementations are grouped by entity types in their own sub-modules
#[async_trait::async_trait]
impl Store for SqliteStore {
    fn identifier(&self) -> &str {
        &self.database_filepath
    }

    fn get_current_timestamp(&self) -> Option<u64> {
        Some(current_timestamp_u64())
    }

    async fn get_note_tags(&self) -> Result<Vec<NoteTagRecord>, StoreError> {
        self.interact_with_connection(SqliteStore::get_note_tags).await
    }

    async fn get_unique_note_tags(&self) -> Result<BTreeSet<NoteTag>, StoreError> {
        self.interact_with_connection(SqliteStore::get_unique_note_tags).await
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
        let smt_forest = self.smt_forest.clone();
        self.interact_with_connection(move |conn| {
            SqliteStore::apply_state_sync(conn, &smt_forest, state_sync_update)
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
        let smt_forest = self.smt_forest.clone();
        self.interact_with_connection(move |conn| {
            SqliteStore::apply_transaction(conn, &smt_forest, &tx_update)
        })
        .await
    }

    async fn apply_transaction_batch(
        &self,
        tx_updates: Vec<TransactionStoreUpdate>,
    ) -> Result<(), StoreError> {
        let smt_forest = self.smt_forest.clone();
        self.interact_with_connection(move |conn| {
            SqliteStore::apply_transaction_batch(conn, &smt_forest, &tx_updates)
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

    async fn get_input_note_by_offset(
        &self,
        filter: NoteFilter,
        consumer: AccountId,
        block_start: Option<BlockNumber>,
        block_end: Option<BlockNumber>,
        offset: u32,
    ) -> Result<Option<InputNoteRecord>, StoreError> {
        self.interact_with_connection(move |conn| {
            SqliteStore::get_input_note_by_offset(
                conn,
                &filter,
                consumer,
                block_start,
                block_end,
                offset,
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
            SqliteStore::prune_irrelevant_blocks(conn, &blocks_to_untrack, &node_indices_to_remove)
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
        Ok(self
            .interact_with_connection(move |conn| {
                SqliteStore::get_block_headers(conn, &block_numbers)
            })
            .await?)
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
        let smt_forest = self.smt_forest.clone();

        self.interact_with_connection(move |conn| {
            SqliteStore::insert_account(
                conn,
                &smt_forest,
                &cloned_account,
                &initial_address,
                client_account_type,
            )
        })
        .await
    }

    async fn update_account(&self, account: &Account) -> Result<(), StoreError> {
        let cloned_account = account.clone();
        let smt_forest = self.smt_forest.clone();

        self.interact_with_connection(move |conn| {
            SqliteStore::update_account(conn, &smt_forest, &cloned_account)
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

    async fn set_setting(&self, key: String, value: Vec<u8>) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            set_setting(conn, &key, &value).into_store_error()
        })
        .await
    }

    async fn get_setting(&self, key: String) -> Result<Option<Vec<u8>>, StoreError> {
        self.interact_with_connection(move |conn| get_setting(conn, &key)).await
    }

    async fn remove_setting(&self, key: String) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| remove_setting(conn, &key)).await
    }

    async fn list_setting_keys(&self) -> Result<Vec<String>, StoreError> {
        self.interact_with_connection(move |conn| list_setting_keys(conn)).await
    }

    async fn apply_settings_mutations(
        &self,
        mutations: Vec<SettingMutation>,
    ) -> Result<(), StoreError> {
        self.interact_with_connection(move |conn| {
            let tx = conn.transaction().into_store_error()?;
            for mutation in &mutations {
                match mutation {
                    SettingMutation::Set { key, value } => {
                        set_setting(&tx, key, value).into_store_error()?;
                    },
                    SettingMutation::Remove { key } => remove_setting(&tx, key)?,
                }
            }
            tx.commit().into_store_error()?;
            Ok(())
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

    async fn get_account_asset(
        &self,
        account_id: AccountId,
        vault_key: AssetVaultKey,
    ) -> Result<Option<(Asset, AssetWitness)>, StoreError> {
        let smt_forest = self.smt_forest.clone();
        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_asset(conn, &smt_forest, account_id, vault_key)
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

    async fn get_account_map_item(
        &self,
        account_id: AccountId,
        slot_name: StorageSlotName,
        key: StorageMapKey,
    ) -> Result<(Word, StorageMapWitness), StoreError> {
        let smt_forest = self.smt_forest.clone();

        self.interact_with_connection(move |conn| {
            SqliteStore::get_account_map_item(conn, &smt_forest, account_id, slot_name, key)
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
            let tx = conn.transaction().into_store_error()?;
            SqliteStore::insert_address(&tx, &address, account_id)?;
            tx.commit().into_store_error()
        })
        .await
    }

    async fn remove_address(&self, address: Address) -> Result<(), StoreError> {
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

/// Returns the current UTC timestamp as `u64` (non-leap seconds since Unix epoch).
pub(crate) fn current_timestamp_u64() -> u64 {
    let now = chrono::Utc::now();
    u64::try_from(now.timestamp()).expect("timestamp is always after epoch")
}

/// Gets a `u64` value from the database.
///
/// `Sqlite` uses `i64` as its internal representation format, and so when retrieving
/// we need to make sure we cast as `u64` to get the original value
pub fn column_value_as_u64<I: rusqlite::RowIndex>(
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
pub fn u64_to_value(v: u64) -> Value {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "We store u64 as i64 as sqlite only allows the latter."
    )]
    Value::Integer(v as i64)
}

// TESTS
// ================================================================================================

#[cfg(test)]
pub mod tests {
    use std::boxed::Box;

    use miden_client::store::Store;
    use miden_client::testing::common::create_test_store_path;

    use super::SqliteStore;

    fn assert_send_sync<T: Send + Sync>() {}

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

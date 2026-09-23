#![allow(clippy::items_after_statements)]

use std::boxed::Box;
use std::string::{String, ToString};
use std::vec::Vec;

use miden_client::Word;
use miden_client::account::AccountId;
use miden_client::note::ToInputNoteCommitments;
use miden_client::store::{StoreError, TransactionFilter, TransactionFilterQuery};
use miden_client::transaction::{
    TransactionDetails,
    TransactionId,
    TransactionRecord,
    TransactionScript,
    TransactionStatus,
    TransactionStatusVariant,
    TransactionStoreUpdate,
};
use miden_client::utils::{Deserializable as _, Serializable as _};
use rusqlite::{Connection, ToSql, Transaction, params};

use super::SqliteStore;
use super::note::apply_note_updates_tx;
use super::sync::add_note_tag_tx;
use crate::forest::{ScopedAccountForest, SqliteForestBackend};
use crate::sql_error::SqlResultExt;
use crate::{blob_array, insert_sql, subst, with_write_tx};

pub(crate) const UPSERT_TRANSACTION_QUERY: &str = insert_sql!(
    transactions {
        id,
        details,
        script_root,
        status_variant,
        status
    } | REPLACE
);

pub(crate) const INSERT_TRANSACTION_SCRIPT_QUERY: &str =
    insert_sql!(transaction_scripts { script_root, script } | IGNORE);

/// The column aliases match the names that [`SqliteStore::get_transactions`] reads.
const TRANSACTIONS_BASE_QUERY: &str = "SELECT \
     tx.id AS id, \
     script.script AS script, \
     tx.details AS details, \
     tx.status AS status \
     FROM transactions AS tx \
     LEFT JOIN transaction_scripts AS script ON tx.script_root = script.script_root";

/// Returns the transactions query for a [`TransactionFilter`], and the values it binds.
///
/// The ids of [`TransactionFilter::Ids`] are bound as one `rarray(?)` value, so the SQL text stays
/// constant for any number of ids.
fn transaction_filter_to_query(filter: &TransactionFilter) -> (String, Vec<Box<dyn ToSql>>) {
    match filter {
        TransactionFilter::All => (TRANSACTIONS_BASE_QUERY.to_string(), vec![]),
        TransactionFilter::Uncommitted => (
            format!(
                "{TRANSACTIONS_BASE_QUERY} WHERE tx.status_variant = {}",
                TransactionStatusVariant::Pending as u8
            ),
            vec![],
        ),
        TransactionFilter::Ids(ids) => (
            format!("{TRANSACTIONS_BASE_QUERY} WHERE tx.id IN rarray(?)"),
            vec![Box::new(blob_array(ids))],
        ),
        TransactionFilter::Query(query) => transaction_query_to_sql(query),
    }
}

/// Returns the query for a [`TransactionFilterQuery`], and the values it binds.
///
/// The account ID and the creation time are read from fixed positions of the serialized details, so
/// the query does not decode the rows it skips.
fn transaction_query_to_sql(query: &TransactionFilterQuery) -> (String, Vec<Box<dyn ToSql>>) {
    let mut conditions = Vec::new();
    let mut params: Vec<Box<dyn ToSql>> = Vec::new();

    if let Some(account_id) = query.account_id {
        // The account ID is the first field of the serialized details and has a fixed length.
        conditions.push(format!("substr(tx.details, 1, {}) = ?", AccountId::SERIALIZED_SIZE));
        params.push(Box::new(account_id.to_bytes()));
    }
    // The status is written into the SQL text and not bound, so that a filter on pending
    // transactions can use the partial index on them.
    if let Some(status) = query.status {
        conditions.push(format!("tx.status_variant = {}", status as u8));
    }

    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    let limit_clause = match query.limit {
        Some(limit) => {
            params.push(Box::new(limit));
            " LIMIT ?"
        },
        None => "",
    };

    let sql = format!(
        "{TRANSACTIONS_BASE_QUERY}{where_clause} ORDER BY {}, tx.id DESC{limit_clause}",
        creation_timestamp_order_sql()
    );

    (sql, params)
}

/// Returns the `ORDER BY` terms that sort transactions from the newest to the oldest.
///
/// The creation timestamp is a little-endian `u64` at the end of the serialized details. One term
/// per byte compares the bytes from the most significant to the least significant one, which is the
/// order of the numbers they encode.
fn creation_timestamp_order_sql() -> String {
    (1..=size_of::<u64>())
        .map(|byte| format!("substr(tx.details, -{byte}, 1) DESC"))
        .collect::<Vec<_>>()
        .join(", ")
}

// TRANSACTIONS
// ================================================================================================

impl SqliteStore {
    /// Retrieves tracked transactions, filtered by [`TransactionFilter`].
    pub fn get_transactions(
        conn: &mut Connection,
        filter: &TransactionFilter,
    ) -> Result<Vec<TransactionRecord>, StoreError> {
        let (query, params) = transaction_filter_to_query(filter);

        conn.prepare(&query)
            .into_store_error()?
            .query_map(rusqlite::params_from_iter(params), |row| {
                Ok((
                    row.get::<_, Vec<u8>>("id")?,
                    row.get::<_, Option<Vec<u8>>>("script")?,
                    row.get::<_, Vec<u8>>("details")?,
                    row.get::<_, Vec<u8>>("status")?,
                ))
            })
            .into_store_error()?
            .map(|result| {
                let (id, script, details, status) = result.into_store_error()?;
                Ok(TransactionRecord {
                    id: TransactionId::read_from_bytes(&id)?,
                    details: TransactionDetails::read_from_bytes(&details)?,
                    script: script
                        .map(|script| TransactionScript::read_from_bytes(&script))
                        .transpose()?,
                    status: TransactionStatus::read_from_bytes(&status)?,
                })
            })
            .collect::<Result<Vec<TransactionRecord>, _>>()
    }

    /// Inserts a transaction and updates the current state based on the `tx_result` changes.
    ///
    /// SQL writes and forest mutations go through the same rusqlite transaction, so they commit or
    /// roll back atomically.
    pub(crate) fn apply_transaction(
        conn: &mut Connection,
        tx_update: &TransactionStoreUpdate,
    ) -> Result<(), StoreError> {
        with_write_tx(conn, |tx| {
            let mut forest = ScopedAccountForest::new(SqliteForestBackend::new(tx))?;
            Self::apply_transaction_in_txn(tx, &mut forest, tx_update)
        })
    }

    /// Applies a batch of [`TransactionStoreUpdate`]s atomically. Either every update in the slice
    /// is persisted or none are. Executes in order inside a single [`rusqlite::Transaction`].
    pub(crate) fn apply_transaction_batch(
        conn: &mut Connection,
        tx_updates: &[TransactionStoreUpdate],
    ) -> Result<(), StoreError> {
        with_write_tx(conn, |tx| {
            let mut forest = ScopedAccountForest::new(SqliteForestBackend::new(tx))?;
            for update in tx_updates {
                Self::apply_transaction_in_txn(tx, &mut forest, update)?;
            }
            Ok(())
        })
    }

    /// Applies a transaction's store update within the provided rusqlite transaction. Does NOT
    /// commit — caller is responsible for commit/rollback.
    ///
    /// The storage-map-root pre-read is performed via the transaction so that each call sees writes
    /// made by prior calls within the same outer transaction.
    pub(crate) fn apply_transaction_in_txn(
        db_tx: &Transaction<'_>,
        smt_forest: &mut ScopedAccountForest<'_, '_>,
        tx_update: &TransactionStoreUpdate,
    ) -> Result<(), StoreError> {
        let executed_transaction = tx_update.executed_transaction();
        let account_patch = executed_transaction.account_patch();

        // Build transaction record
        let nullifiers: Vec<Word> = executed_transaction
            .input_notes()
            .iter()
            .map(|x| x.nullifier().as_word())
            .collect();

        let output_notes = executed_transaction.output_notes();

        let details = TransactionDetails {
            account_id: executed_transaction.account_id(),
            init_account_state: executed_transaction.initial_account().initial_commitment(),
            final_account_state: executed_transaction.final_account().to_commitment(),
            input_note_nullifiers: nullifiers,
            output_notes: output_notes.clone(),
            block_num: executed_transaction.block_header().block_num(),
            submission_height: tx_update.submission_height(),
            expiration_block_num: executed_transaction.expiration_block_num(),
            creation_timestamp: super::current_timestamp_u64(),
        };

        let transaction_record = TransactionRecord::new(
            executed_transaction.id(),
            details,
            executed_transaction.tx_args().tx_script().cloned(),
            TransactionStatus::Pending,
        );

        // Insert transaction data
        upsert_transaction_record(db_tx, &transaction_record)?;

        // Account Data
        Self::apply_account_patch(
            db_tx,
            smt_forest,
            &executed_transaction.initial_account().into(),
            executed_transaction.final_account(),
            account_patch,
        )?;

        // Note Updates
        apply_note_updates_tx(db_tx, tx_update.note_updates())?;

        // Note tags
        for tag_record in tx_update.new_tags() {
            add_note_tag_tx(db_tx, tag_record)?;
        }

        Ok(())
    }
}

/// Updates the transaction record in the database, inserting it if it doesn't exist.
pub(crate) fn upsert_transaction_record(
    tx: &Transaction<'_>,
    transaction: &TransactionRecord,
) -> Result<(), StoreError> {
    let script_root = transaction.script.as_ref().map(|script| script.root().to_bytes());

    if let Some(script) = &transaction.script {
        tx.execute(INSERT_TRANSACTION_SCRIPT_QUERY, params![script_root, script.to_bytes()])
            .into_store_error()?;
    }

    tx.execute(
        UPSERT_TRANSACTION_QUERY,
        params![
            transaction.id.to_bytes(),
            transaction.details.to_bytes(),
            script_root,
            transaction.status.variant() as u8,
            transaction.status.to_bytes(),
        ],
    )
    .into_store_error()?;

    Ok(())
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::store::{TransactionFilter, TransactionFilterQuery};
    use miden_client::transaction::{
        DiscardCause,
        RawOutputNotes,
        TransactionDetails,
        TransactionId,
        TransactionRecord,
        TransactionStatus,
        TransactionStatusVariant,
    };
    use miden_client::{Felt, Word, ZERO};
    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
    };
    use rusqlite::Connection;

    use super::{SqliteStore, transaction_filter_to_query, upsert_transaction_record};
    use crate::db_management::migration::SqliteMigrator;

    /// Builds a script-less transaction record with the given status.
    fn create_transaction_record(index: u64, status: TransactionStatus) -> TransactionRecord {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        create_account_transaction_record(index, account_id, 0, status)
    }

    /// Builds a script-less transaction record that `account_id` created at `creation_timestamp`.
    fn create_account_transaction_record(
        index: u64,
        account_id: AccountId,
        creation_timestamp: u64,
        status: TransactionStatus,
    ) -> TransactionRecord {
        const BLOCK_NUM: u32 = 5;

        let details = TransactionDetails {
            account_id,
            init_account_state: Word::default(),
            final_account_state: Word::default(),
            input_note_nullifiers: vec![],
            output_notes: RawOutputNotes::new(vec![]).unwrap(),
            block_num: BlockNumber::from(BLOCK_NUM),
            submission_height: BlockNumber::from(BLOCK_NUM),
            expiration_block_num: BlockNumber::from(BLOCK_NUM + 1),
            creation_timestamp,
        };

        let id = TransactionId::from_raw([Felt::new_unchecked(index), ZERO, ZERO, ZERO].into());

        TransactionRecord::new(id, details, None, status)
    }

    /// Returns the IDs of the transactions `query` selects, in the order the store returns them.
    fn query_ids(conn: &mut Connection, query: TransactionFilterQuery) -> Vec<TransactionId> {
        SqliteStore::get_transactions(conn, &TransactionFilter::Query(query))
            .unwrap()
            .iter()
            .map(|record| record.id)
            .collect()
    }

    fn create_test_connection(records: &[TransactionRecord]) -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        SqliteMigrator::client().apply(&mut conn).unwrap();

        let db_tx = conn.transaction().unwrap();
        for record in records {
            upsert_transaction_record(&db_tx, record).unwrap();
        }
        db_tx.commit().unwrap();

        conn
    }

    /// Returns the `detail` column of every step of the query plan for `query`.
    fn query_plan(conn: &Connection, query: &str) -> Vec<String> {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {query}")).unwrap();
        stmt.query_map([], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn uncommitted_returns_only_pending_transactions() {
        let pending = create_transaction_record(1, TransactionStatus::Pending);
        let committed = create_transaction_record(
            2,
            TransactionStatus::Committed {
                block_number: BlockNumber::from(6u32),
                commit_timestamp: 0,
            },
        );
        let discarded =
            create_transaction_record(3, TransactionStatus::Discarded(DiscardCause::Expired));

        let mut conn = create_test_connection(&[pending.clone(), committed, discarded]);

        let records =
            SqliteStore::get_transactions(&mut conn, &TransactionFilter::Uncommitted).unwrap();

        let ids: Vec<_> = records.iter().map(|record| record.id).collect();
        assert_eq!(ids, vec![pending.id]);
    }

    #[test]
    fn uncommitted_is_served_by_the_pending_transactions_index() {
        let conn = create_test_connection(&[]);

        let (query, _) = transaction_filter_to_query(&TransactionFilter::Uncommitted);
        let plan = query_plan(&conn, &query).join("\n");

        // Every entry of the partial index is a pending transaction, so the search never touches a
        // committed or discarded row.
        assert!(
            plan.contains("SEARCH tx USING INDEX idx_transactions_pending (status_variant=?)"),
            "pending transactions must be read from the partial index: {plan}"
        );
    }

    #[test]
    fn query_keeps_only_the_given_account_and_status() {
        let account_a =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let account_b =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let committed = TransactionStatus::Committed {
            block_number: BlockNumber::from(6u32),
            commit_timestamp: 0,
        };

        let a_pending =
            create_account_transaction_record(1, account_a, 10, TransactionStatus::Pending);
        let a_committed = create_account_transaction_record(2, account_a, 20, committed);
        let b_pending =
            create_account_transaction_record(3, account_b, 30, TransactionStatus::Pending);
        let mut conn =
            create_test_connection(&[a_pending.clone(), a_committed.clone(), b_pending.clone()]);

        let by_account = TransactionFilterQuery {
            account_id: Some(account_a),
            ..Default::default()
        };
        assert_eq!(query_ids(&mut conn, by_account), vec![a_committed.id, a_pending.id]);

        let by_status = TransactionFilterQuery {
            status: Some(TransactionStatusVariant::Pending),
            ..Default::default()
        };
        assert_eq!(query_ids(&mut conn, by_status), vec![b_pending.id, a_pending.id]);

        let by_both = TransactionFilterQuery {
            account_id: Some(account_a),
            status: Some(TransactionStatusVariant::Pending),
            limit: None,
        };
        assert_eq!(query_ids(&mut conn, by_both), vec![a_pending.id]);
    }

    #[test]
    fn query_binds_a_condition_and_the_limit_in_the_order_the_query_lists_them() {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let other_account =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();

        let older =
            create_account_transaction_record(1, account_id, 10, TransactionStatus::Pending);
        let newer =
            create_account_transaction_record(2, account_id, 20, TransactionStatus::Pending);
        let other =
            create_account_transaction_record(3, other_account, 30, TransactionStatus::Pending);
        let mut conn = create_test_connection(&[older.clone(), newer.clone(), other]);

        // The account ID binds to the condition and the count to the limit, so a swap of the two
        // values selects other transactions.
        let query = TransactionFilterQuery {
            account_id: Some(account_id),
            status: None,
            limit: Some(1),
        };
        assert_eq!(query_ids(&mut conn, query), vec![newer.id]);
    }

    #[test]
    fn query_orders_by_creation_time_newest_first_and_limits() {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        // 255 and 256 compare the other way round as raw little-endian bytes, so only a numeric
        // order puts 256 first.
        let at_255 =
            create_account_transaction_record(1, account_id, 255, TransactionStatus::Pending);
        let at_1 = create_account_transaction_record(2, account_id, 1, TransactionStatus::Pending);
        let at_256 =
            create_account_transaction_record(3, account_id, 256, TransactionStatus::Pending);
        let mut conn = create_test_connection(&[at_255.clone(), at_1.clone(), at_256.clone()]);

        assert_eq!(
            query_ids(&mut conn, TransactionFilterQuery::default()),
            vec![at_256.id, at_255.id, at_1.id]
        );

        let newest_two = TransactionFilterQuery { limit: Some(2), ..Default::default() };
        assert_eq!(query_ids(&mut conn, newest_two), vec![at_256.id, at_255.id]);
    }
}

#![allow(clippy::items_after_statements)]

use std::boxed::Box;
use std::string::{String, ToString};
use std::vec::Vec;

use miden_client_core::errors::StoreError;
use miden_client_core::store::{TransactionFilter, TransactionFilterQuery};
use miden_client_core::transaction::{
    BatchStoreUpdate,
    TransactionDetails,
    TransactionRecord,
    TransactionStatus,
    TransactionStatusVariant,
    TransactionStoreUpdate,
};
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::transaction::{
    ExecutedTransaction,
    LogTopic,
    ToInputNoteCommitments,
    TransactionId,
    TransactionLog,
    TransactionLogs,
};
use miden_protocol::utils::serde::{Deserializable, DeserializationError, Serializable};
use miden_protocol::{Felt, Word};
use rusqlite::{Connection, ToSql, Transaction, params};

use super::SqliteStore;
use super::note::apply_note_updates_tx;
use super::sync::add_note_tag_tx;
use crate::forest::{ScopedAccountForest, SqliteForestBackend};
use crate::sql_error::SqlResultExt;
use crate::{blob_array, insert_sql, proto, subst, u64_to_value, with_write_tx, word_from_blob};

pub(crate) const UPSERT_TRANSACTION_QUERY: &str = insert_sql!(
    transactions {
        id,
        details,
        script_root,
        status_variant,
        status
    } | REPLACE
);

const INSERT_TRANSACTION_LOG_QUERY: &str = insert_sql!(
    transaction_logs {
        transaction_id,
        log_index,
        emitter,
        topic,
        content
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
fn transaction_filter_to_query(
    filter: &TransactionFilter,
) -> Result<(String, Vec<Box<dyn ToSql>>), StoreError> {
    let query = match filter {
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
            vec![Box::new(blob_array(ids.iter().map(TransactionId::as_bytes))) as Box<dyn ToSql>],
        ),
        TransactionFilter::Query(query) => transaction_query_to_sql(query),
        // `TransactionFilter` is `#[non_exhaustive]`, so a new variant must not compile silently
        // into this store without an SQL mapping.
        _ => {
            return Err(StoreError::QueryError(
                "unsupported transaction filter variant".to_string(),
            ));
        },
    };
    Ok(query)
}

/// Returns the query for a [`TransactionFilterQuery`], and the values it binds.
///
/// The status is the only part of the query that maps to a table column. The account ID and the
/// creation time are fields of the proto-encoded details blob, so [`SqliteStore::get_transactions`]
/// filters by account, orders by creation time, and applies the limit in Rust after it decodes each
/// row.
fn transaction_query_to_sql(query: &TransactionFilterQuery) -> (String, Vec<Box<dyn ToSql>>) {
    // The status is written into the SQL text and not bound, so that a filter on pending
    // transactions can use the partial index on them.
    let where_clause = match query.status {
        Some(status) => format!(" WHERE tx.status_variant = {}", status as u8),
        None => String::new(),
    };

    (format!("{TRANSACTIONS_BASE_QUERY}{where_clause}"), Vec::new())
}

// TRANSACTIONS
// ================================================================================================

impl SqliteStore {
    /// Retrieves tracked transactions, filtered by [`TransactionFilter`].
    pub fn get_transactions(
        conn: &mut Connection,
        filter: &TransactionFilter,
    ) -> Result<Vec<TransactionRecord>, StoreError> {
        let (query, params) = transaction_filter_to_query(filter)?;

        let mut records = conn
            .prepare(&query)
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
                    id: TransactionId::from_raw(word_from_blob(&id)?),
                    details: proto::decode_unchecked(&details)?,
                    script: script.map(|script| proto::decode_unchecked(&script)).transpose()?,
                    status: proto::decode_unchecked(&status)?,
                })
            })
            .collect::<Result<Vec<TransactionRecord>, StoreError>>()?;

        // The account ID, the creation time, and the limit are not table columns, so they are
        // applied after each row is decoded. Only the query filter uses them.
        if let TransactionFilter::Query(query) = filter {
            if let Some(account_id) = query.account_id {
                records.retain(|record| record.details.account_id == account_id);
            }
            // Order the transactions from the newest to the oldest. The transaction ID breaks a tie
            // between equal creation times, so the order is stable.
            records.sort_by(|a, b| {
                b.details
                    .creation_timestamp
                    .cmp(&a.details.creation_timestamp)
                    .then_with(|| b.id.as_bytes().cmp(&a.id.as_bytes()))
            });
            if let Some(limit) = query.limit {
                records.truncate(limit as usize);
            }
        }

        Ok(records)
    }

    /// Retrieves the logs that a stored transaction emitted, in emission order.
    ///
    /// Returns `None` if the store does not hold the transaction.
    pub fn get_transaction_logs(
        conn: &mut Connection,
        transaction_id: TransactionId,
    ) -> Result<Option<TransactionLogs>, StoreError> {
        const TRANSACTION_EXISTS_QUERY: &str =
            "SELECT EXISTS (SELECT 1 FROM transactions WHERE id = ?)";
        const LOGS_QUERY: &str = "SELECT emitter, topic, content FROM transaction_logs \
             WHERE transaction_id = ? ORDER BY log_index";

        // A transaction without logs has no rows in the logs table, so the transactions table tells
        // whether the store holds the transaction.
        let is_stored: bool = conn
            .query_row(TRANSACTION_EXISTS_QUERY, params![transaction_id.as_bytes()], |row| {
                row.get(0)
            })
            .into_store_error()?;
        if !is_stored {
            return Ok(None);
        }

        let logs = conn
            .prepare_cached(LOGS_QUERY)
            .into_store_error()?
            .query_map(params![transaction_id.as_bytes()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>("emitter")?,
                    row.get::<_, Vec<u8>>("topic")?,
                    row.get::<_, Vec<u8>>("content")?,
                ))
            })
            .into_store_error()?
            .map(|result| {
                let (emitter, topic, content) = result.into_store_error()?;
                TransactionLog::new(
                    AccountId::read_from_bytes(&emitter)?,
                    log_topic_from_blob(&topic)?,
                    log_content_from_blob(&content)?,
                )
                .map_err(|err| invalid_logs(&err))
            })
            .collect::<Result<Vec<TransactionLog>, StoreError>>()?;

        TransactionLogs::new(logs).map(Some).map_err(|err| invalid_logs(&err))
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

    /// Applies a [`BatchStoreUpdate`] atomically inside a single [`rusqlite::Transaction`]. Either
    /// the whole update is persisted or nothing is.
    ///
    /// Each transaction is inserted and its account patch applied in batch order. The merged note
    /// updates and tags are applied once, after all the transactions.
    pub(crate) fn apply_transaction_batch(
        conn: &mut Connection,
        batch_update: &BatchStoreUpdate,
    ) -> Result<(), StoreError> {
        with_write_tx(conn, |tx| {
            let mut forest = ScopedAccountForest::new(SqliteForestBackend::new(tx))?;
            for executed_transaction in batch_update.executed_transactions() {
                Self::insert_transaction_in_txn(
                    tx,
                    &mut forest,
                    executed_transaction,
                    batch_update.submission_height(),
                )?;
            }

            apply_note_updates_tx(tx, batch_update.note_updates())?;
            for tag_record in batch_update.new_tags() {
                add_note_tag_tx(tx, tag_record)?;
            }

            Ok(())
        })
    }

    /// Applies a transaction's store update within the provided rusqlite transaction. Does NOT
    /// commit — caller is responsible for commit/rollback.
    pub(crate) fn apply_transaction_in_txn(
        db_tx: &Transaction<'_>,
        smt_forest: &mut ScopedAccountForest<'_, '_>,
        tx_update: &TransactionStoreUpdate,
    ) -> Result<(), StoreError> {
        Self::insert_transaction_in_txn(
            db_tx,
            smt_forest,
            tx_update.executed_transaction(),
            tx_update.submission_height(),
        )?;

        // Note Updates
        apply_note_updates_tx(db_tx, tx_update.note_updates())?;

        // Note tags
        for tag_record in tx_update.new_tags() {
            add_note_tag_tx(db_tx, tag_record)?;
        }

        Ok(())
    }

    /// Inserts the record of an executed transaction and applies its account patch within the
    /// provided rusqlite transaction.
    ///
    /// The storage-map-root pre-read is performed via the transaction so that each call sees writes
    /// made by prior calls within the same outer transaction.
    fn insert_transaction_in_txn(
        db_tx: &Transaction<'_>,
        smt_forest: &mut ScopedAccountForest<'_, '_>,
        executed_transaction: &ExecutedTransaction,
        submission_height: BlockNumber,
    ) -> Result<(), StoreError> {
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
            submission_height,
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
        insert_transaction_logs(db_tx, executed_transaction.id(), executed_transaction.logs())?;

        // Account Data
        Self::apply_account_patch(
            db_tx,
            smt_forest,
            &executed_transaction.initial_account().into(),
            executed_transaction.final_account(),
            account_patch,
        )
    }
}

/// Updates the transaction record in the database, inserting it if it doesn't exist.
pub(crate) fn upsert_transaction_record(
    tx: &Transaction<'_>,
    transaction: &TransactionRecord,
) -> Result<(), StoreError> {
    let script_root = transaction.script.as_ref().map(|script| script.root().as_bytes());

    if let Some(script) = &transaction.script {
        tx.execute(INSERT_TRANSACTION_SCRIPT_QUERY, params![script_root, proto::encode(script)])
            .into_store_error()?;
    }

    tx.execute(
        UPSERT_TRANSACTION_QUERY,
        params![
            transaction.id.as_bytes(),
            proto::encode(&transaction.details),
            script_root,
            transaction.status.variant() as u8,
            proto::encode(&transaction.status),
        ],
    )
    .into_store_error()?;

    Ok(())
}

// TRANSACTION LOGS
// ================================================================================================

/// The size in bytes of an encoded log topic: the suffix and the prefix as little-endian `u64`.
const LOG_TOPIC_SIZE: usize = 16;

/// Inserts the logs of a transaction, one row for each log, in emission order.
///
/// The row of the transaction must exist before this call.
pub(crate) fn insert_transaction_logs(
    tx: &Transaction<'_>,
    transaction_id: TransactionId,
    logs: &TransactionLogs,
) -> Result<(), StoreError> {
    let mut stmt = tx.prepare_cached(INSERT_TRANSACTION_LOG_QUERY).into_store_error()?;

    for (log_index, log) in logs.iter().enumerate() {
        let content: Vec<u8> = log.content().iter().flat_map(Word::as_bytes).collect();
        stmt.execute(params![
            transaction_id.as_bytes(),
            u64_to_value(log_index as u64),
            log.emitter().to_bytes(),
            log_topic_to_blob(log.topic()),
            content,
        ])
        .into_store_error()?;
    }

    Ok(())
}

fn log_topic_to_blob(topic: LogTopic) -> Vec<u8> {
    let mut blob = Vec::with_capacity(LOG_TOPIC_SIZE);
    blob.extend_from_slice(&topic.suffix().as_canonical_u64().to_le_bytes());
    blob.extend_from_slice(&topic.prefix().as_canonical_u64().to_le_bytes());
    blob
}

fn log_topic_from_blob(blob: &[u8]) -> Result<LogTopic, StoreError> {
    let felt = |bytes: &[u8]| {
        let value = u64::from_le_bytes(bytes.try_into().expect("the chunk holds eight bytes"));
        Felt::try_from(value).map_err(|err| invalid_logs(&err))
    };

    if blob.len() != LOG_TOPIC_SIZE {
        return Err(invalid_logs(&format!("log topic has {} bytes", blob.len())));
    }
    let (suffix, prefix) = blob.split_at(LOG_TOPIC_SIZE / 2);

    Ok(LogTopic::new(felt(suffix)?, felt(prefix)?))
}

fn log_content_from_blob(blob: &[u8]) -> Result<Vec<Word>, StoreError> {
    let (words, remainder) = blob.as_chunks::<{ Word::SERIALIZED_SIZE }>();
    if !remainder.is_empty() {
        return Err(invalid_logs(&format!("log content has {} bytes", blob.len())));
    }

    words.iter().map(|word| word_from_blob(word)).collect()
}

fn invalid_logs(err: &impl ToString) -> StoreError {
    StoreError::DataDeserializationError(DeserializationError::InvalidValue(err.to_string()))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::transaction::{
        LogTopic,
        RawOutputNotes,
        TransactionId,
        TransactionLog,
        TransactionLogs,
    };
    use miden_client::{Felt, Word, ZERO};
    use miden_client_core::store::{TransactionFilter, TransactionFilterQuery};
    use miden_client_core::transaction::{
        DiscardCause,
        TransactionDetails,
        TransactionRecord,
        TransactionStatus,
        TransactionStatusVariant,
    };
    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
    };
    use rusqlite::Connection;

    use super::{
        SqliteStore,
        insert_transaction_logs,
        transaction_filter_to_query,
        upsert_transaction_record,
    };
    use crate::db_management::migration::SqliteMigrator;
    use crate::sql_error::SqlResultExt;

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

    fn test_logs() -> TransactionLogs {
        let emitter =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let topic = LogTopic::from_name("miden::testing::topic");
        let other_topic = LogTopic::from_name("miden::testing::other_topic");
        TransactionLogs::new(vec![
            TransactionLog::new(emitter, topic, vec![Word::from([1u32, 2, 3, 4])]).unwrap(),
            TransactionLog::new(emitter, other_topic, vec![]).unwrap(),
            // A duplicate of the first log. The commitment covers the order and the duplicates.
            TransactionLog::new(emitter, topic, vec![Word::from([1u32, 2, 3, 4])]).unwrap(),
            TransactionLog::new(emitter, topic, vec![Word::from([5u32; 4]), Word::from([6u32; 4])])
                .unwrap(),
        ])
        .unwrap()
    }

    fn insert_logs(conn: &mut Connection, id: TransactionId, logs: &TransactionLogs) {
        let db_tx = conn.transaction().unwrap();
        insert_transaction_logs(&db_tx, id, logs).unwrap();
        db_tx.commit().unwrap();
    }

    /// The logs table must return the logs of each transaction, in emission order.
    #[test]
    fn transaction_logs_round_trip() {
        let logs = test_logs();
        let without_logs = create_transaction_record(1, TransactionStatus::Pending);
        let mut with_logs = create_transaction_record(2, TransactionStatus::Pending);

        let mut conn = create_test_connection(&[without_logs.clone(), with_logs.clone()]);
        insert_logs(&mut conn, with_logs.id, &logs);

        let stored = SqliteStore::get_transaction_logs(&mut conn, with_logs.id).unwrap().unwrap();
        assert_eq!(stored, logs);
        assert_eq!(stored.commitment(), logs.commitment());

        let empty = SqliteStore::get_transaction_logs(&mut conn, without_logs.id).unwrap();
        assert_eq!(empty, Some(TransactionLogs::default()));

        // A status update replaces the row of the transaction and must keep the logs. The store
        // enforces foreign keys, so the replace must also pass with them enabled.
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();
        with_logs.commit_transaction(BlockNumber::from(6u32), 0);
        let db_tx = conn.transaction().unwrap();
        upsert_transaction_record(&db_tx, &with_logs).unwrap();
        db_tx.commit().unwrap();
        assert_eq!(SqliteStore::get_transaction_logs(&mut conn, with_logs.id).unwrap(), Some(logs));
    }

    #[test]
    fn transaction_logs_of_an_unknown_transaction_are_none() {
        let record = create_transaction_record(1, TransactionStatus::Pending);
        let unknown = create_transaction_record(2, TransactionStatus::Pending);
        let mut conn = create_test_connection(&[record]);

        assert_eq!(SqliteStore::get_transaction_logs(&mut conn, unknown.id).unwrap(), None);
    }

    /// The foreign key must reject the logs of a transaction that the store does not hold.
    #[test]
    fn transaction_logs_need_a_stored_transaction() {
        let unknown = create_transaction_record(1, TransactionStatus::Pending);
        let mut conn = create_test_connection(&[]);
        conn.pragma_update(None, "foreign_keys", "ON").unwrap();

        let db_tx = conn.transaction().unwrap();
        let result = insert_transaction_logs(&db_tx, unknown.id, &test_logs())
            .and_then(|()| db_tx.commit().into_store_error());
        assert!(result.is_err());
    }

    /// Rows that do not decode must fail the read and not return partial logs.
    #[test]
    fn corrupt_transaction_logs_fail_the_read() {
        for column in ["emitter", "topic", "content"] {
            let record = create_transaction_record(1, TransactionStatus::Pending);
            let mut conn = create_test_connection(std::slice::from_ref(&record));
            insert_logs(&mut conn, record.id, &test_logs());
            conn.execute(
                &format!("UPDATE transaction_logs SET {column} = x'ffffff' WHERE log_index = 0"),
                [],
            )
            .unwrap();

            assert!(
                SqliteStore::get_transaction_logs(&mut conn, record.id).is_err(),
                "a corrupt {column} must fail the read"
            );
        }
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

        let (query, _) = transaction_filter_to_query(&TransactionFilter::Uncommitted).unwrap();
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

#![allow(clippy::items_after_statements)]

use std::rc::Rc;
use std::vec::Vec;

use miden_client::Word;
use miden_client::note::ToInputNoteCommitments;
use miden_client::store::{StoreError, TransactionFilter};
use miden_client::transaction::{
    TransactionDetails,
    TransactionId,
    TransactionRecord,
    TransactionScript,
    TransactionStatus,
    TransactionStoreUpdate,
};
use miden_client::utils::{Deserializable as _, Serializable as _};
use rusqlite::types::Value;
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::SqliteStore;
use super::note::apply_note_updates_tx;
use super::sync::add_note_tag_tx;
use crate::forest::{ScopedAccountForest, SqliteForestBackend};
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

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

// TRANSACTIONS
// ================================================================================================

struct SerializedTransactionData {
    /// Transaction ID
    id: Vec<u8>,
    /// Script root
    script_root: Option<Vec<u8>>,
    /// Transaction script
    tx_script: Option<Vec<u8>>,
    /// Transaction details
    details: Vec<u8>,
    /// Transaction status variant identifier
    status_variant: u8,
    /// Serialized transaction status
    status: Vec<u8>,
}

struct SerializedTransactionParts {
    /// Transaction ID
    id: Vec<u8>,
    /// Transaction script
    tx_script: Option<Vec<u8>>,
    /// Transaction details
    details: Vec<u8>,
    /// Serialized transaction status
    status: Vec<u8>,
}

impl SqliteStore {
    /// Retrieves tracked transactions, filtered by [`TransactionFilter`].
    pub fn get_transactions(
        conn: &mut Connection,
        filter: &TransactionFilter,
    ) -> Result<Vec<TransactionRecord>, StoreError> {
        match filter {
            TransactionFilter::Ids(ids) => {
                let id_blobs = ids.iter().map(|id| Value::Blob(id.to_bytes())).collect::<Vec<_>>();

                // Create a prepared statement and bind the array parameter
                conn.prepare(filter.to_query().as_ref())
                    .into_store_error()?
                    .query_map(params![Rc::new(id_blobs)], parse_transaction_columns)
                    .into_store_error()?
                    .map(|result| Ok(result.into_store_error()?).and_then(parse_transaction))
                    .collect::<Result<Vec<TransactionRecord>, _>>()
            },
            _ => {
                // For other filters, no parameters are needed
                conn.prepare(filter.to_query().as_ref())
                    .into_store_error()?
                    .query_map([], parse_transaction_columns)
                    .into_store_error()?
                    .map(|result| Ok(result.into_store_error()?).and_then(parse_transaction))
                    .collect::<Result<Vec<TransactionRecord>, _>>()
            },
        }
    }

    /// Inserts a transaction and updates the current state based on the `tx_result` changes.
    ///
    /// SQL writes and forest mutations go through the same rusqlite transaction, so they commit
    /// or roll back atomically.
    pub fn apply_transaction(
        conn: &mut Connection,
        tx_update: &TransactionStoreUpdate,
    ) -> Result<(), StoreError> {
        let db_tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .into_store_error()?;
        {
            let mut forest = ScopedAccountForest::new(SqliteForestBackend::new(&db_tx))?;
            Self::apply_transaction_in_txn(&db_tx, &mut forest, tx_update)?;
        }
        db_tx.commit().into_store_error()
    }

    /// Applies a batch of [`TransactionStoreUpdate`]s atomically. Either every update in the
    /// slice is persisted or none are. Executes in order inside a single
    /// [`rusqlite::Transaction`].
    pub fn apply_transaction_batch(
        conn: &mut Connection,
        tx_updates: &[TransactionStoreUpdate],
    ) -> Result<(), StoreError> {
        let db_tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .into_store_error()?;
        {
            let mut forest = ScopedAccountForest::new(SqliteForestBackend::new(&db_tx))?;
            for update in tx_updates {
                Self::apply_transaction_in_txn(&db_tx, &mut forest, update)?;
            }
        }
        db_tx.commit().into_store_error()
    }

    /// Applies a transaction's store update within the provided rusqlite transaction.
    /// Does NOT commit — caller is responsible for commit/rollback.
    ///
    /// The storage-map-root pre-read is performed via the transaction so that each call sees
    /// writes made by prior calls within the same outer transaction.
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
    let SerializedTransactionData {
        id,
        script_root,
        tx_script,
        details,
        status_variant,
        status,
    } = serialize_transaction_data(transaction);

    if let Some(root) = script_root.clone() {
        tx.execute(INSERT_TRANSACTION_SCRIPT_QUERY, params![root, tx_script])
            .into_store_error()?;
    }

    tx.execute(
        UPSERT_TRANSACTION_QUERY,
        params![id, details, script_root, status_variant, status],
    )
    .into_store_error()?;

    Ok(())
}

/// Serializes the transaction record into a format suitable for storage in the database.
fn serialize_transaction_data(transaction_record: &TransactionRecord) -> SerializedTransactionData {
    let transaction_id = transaction_record.id.to_bytes();

    let script_root = transaction_record.script.as_ref().map(|script| script.root().to_bytes());
    let tx_script = transaction_record.script.as_ref().map(TransactionScript::to_bytes);

    SerializedTransactionData {
        id: transaction_id,
        script_root,
        tx_script,
        details: transaction_record.details.to_bytes(),
        status_variant: transaction_record.status.variant() as u8,
        status: transaction_record.status.to_bytes(),
    }
}

fn parse_transaction_columns(
    row: &rusqlite::Row<'_>,
) -> Result<SerializedTransactionParts, rusqlite::Error> {
    let id: Vec<u8> = row.get(0)?;
    let tx_script: Option<Vec<u8>> = row.get(1)?;
    let details: Vec<u8> = row.get(2)?;
    let status: Vec<u8> = row.get(3)?;

    Ok(SerializedTransactionParts { id, tx_script, details, status })
}

/// Parse a transaction from the provided parts.
fn parse_transaction(
    serialized_transaction: SerializedTransactionParts,
) -> Result<TransactionRecord, StoreError> {
    let SerializedTransactionParts { id, tx_script, details, status } = serialized_transaction;

    let id = TransactionId::read_from_bytes(&id)?;

    let script: Option<TransactionScript> = tx_script
        .map(|script| TransactionScript::read_from_bytes(&script))
        .transpose()?;

    Ok(TransactionRecord {
        id,
        details: TransactionDetails::read_from_bytes(&details)?,
        script,
        status: TransactionStatus::read_from_bytes(&status)?,
    })
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::store::TransactionFilter;
    use miden_client::transaction::{
        DiscardCause,
        RawOutputNotes,
        TransactionDetails,
        TransactionId,
        TransactionRecord,
        TransactionStatus,
    };
    use miden_client::{Felt, Word, ZERO};
    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;
    use rusqlite::Connection;

    use super::{SqliteStore, upsert_transaction_record};
    use crate::db_management::migration::SqliteMigrator;

    /// Builds a script-less transaction record with the given status.
    fn create_transaction_record(index: u64, status: TransactionStatus) -> TransactionRecord {
        const BLOCK_NUM: u32 = 5;

        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
        let details = TransactionDetails {
            account_id,
            init_account_state: Word::default(),
            final_account_state: Word::default(),
            input_note_nullifiers: vec![],
            output_notes: RawOutputNotes::new(vec![]).unwrap(),
            block_num: BlockNumber::from(BLOCK_NUM),
            submission_height: BlockNumber::from(BLOCK_NUM),
            expiration_block_num: BlockNumber::from(BLOCK_NUM + 1),
            creation_timestamp: 0,
        };

        let id = TransactionId::from_raw([Felt::new_unchecked(index), ZERO, ZERO, ZERO].into());

        TransactionRecord::new(id, details, None, status)
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

        let query = TransactionFilter::Uncommitted.to_query();
        let plan = query_plan(&conn, &query).join("\n");

        // Every entry of the partial index is a pending transaction, so the search never touches
        // a committed or discarded row.
        assert!(
            plan.contains("SEARCH tx USING INDEX idx_transactions_pending (status_variant=?)"),
            "pending transactions must be read from the partial index: {plan}"
        );
    }
}

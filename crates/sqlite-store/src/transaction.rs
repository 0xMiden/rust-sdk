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
    TransactionStatusVariant,
    TransactionStoreUpdate,
};
use miden_client::utils::{Deserializable as _, Serializable as _};
use rusqlite::types::Value;
use rusqlite::{Connection, Transaction, params};

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

/// Returns the transactions query for a [`TransactionFilter`], and the value list it binds.
///
/// Only [`TransactionFilter::Ids`] binds a parameter. The ids are bound as one `rarray(?)` value,
/// so the SQL text stays constant for any number of ids.
fn transaction_filter_to_query(filter: &TransactionFilter) -> (String, Option<Rc<Vec<Value>>>) {
    match filter {
        TransactionFilter::All => (TRANSACTIONS_BASE_QUERY.to_string(), None),
        TransactionFilter::Uncommitted => (
            format!(
                "{TRANSACTIONS_BASE_QUERY} WHERE tx.status_variant = {}",
                TransactionStatusVariant::Pending as u8
            ),
            None,
        ),
        TransactionFilter::Ids(ids) => (
            format!("{TRANSACTIONS_BASE_QUERY} WHERE tx.id IN rarray(?)"),
            Some(blob_array(ids)),
        ),
    }
}

// TRANSACTIONS
// ================================================================================================

impl SqliteStore {
    /// Retrieves tracked transactions, filtered by [`TransactionFilter`].
    pub fn get_transactions(
        conn: &mut Connection,
        filter: &TransactionFilter,
    ) -> Result<Vec<TransactionRecord>, StoreError> {
        let (query, id_list) = transaction_filter_to_query(filter);

        conn.prepare(&query)
            .into_store_error()?
            .query_map(rusqlite::params_from_iter(id_list), |row| {
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

    use super::{SqliteStore, transaction_filter_to_query, upsert_transaction_record};
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

        let (query, _) = transaction_filter_to_query(&TransactionFilter::Uncommitted);
        let plan = query_plan(&conn, &query).join("\n");

        // Every entry of the partial index is a pending transaction, so the search never touches a
        // committed or discarded row.
        assert!(
            plan.contains("SEARCH tx USING INDEX idx_transactions_pending (status_variant=?)"),
            "pending transactions must be read from the partial index: {plan}"
        );
    }
}

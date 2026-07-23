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
use crate::forest::{ScopedAccountForest, forest_backend};
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

pub(crate) const UPSERT_TRANSACTION_QUERY: &str = insert_sql!(
    transactions {
        id,
        details,
        script_root,
        block_num,
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
    /// Block number
    block_num: u32,
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
            let mut forest = ScopedAccountForest::new(forest_backend(&db_tx))?;
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
            let mut forest = ScopedAccountForest::new(forest_backend(&db_tx))?;
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

        let old_map_roots = Self::get_storage_map_roots_for_patch(
            db_tx,
            executed_transaction.account_id(),
            account_patch.storage(),
        )?;

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
            &old_map_roots,
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
        block_num,
        status_variant,
        status,
    } = serialize_transaction_data(transaction);

    if let Some(root) = script_root.clone() {
        tx.execute(INSERT_TRANSACTION_SCRIPT_QUERY, params![root, tx_script])
            .into_store_error()?;
    }

    tx.execute(
        UPSERT_TRANSACTION_QUERY,
        params![id, details, script_root, block_num, status_variant, status],
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
        block_num: transaction_record.details.block_num.as_u32(),
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

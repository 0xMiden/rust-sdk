#![allow(clippy::items_after_statements)]

use std::collections::BTreeSet;
use std::vec::Vec;

use miden_client::Word;
use miden_client::account::AccountId;
use miden_client::note::{BlockNumber, NoteTag};
use miden_client::store::StoreError;
use miden_client::sync::{NoteTagRecord, NoteTagSource, PublicAccountUpdate, StateSyncUpdate};
use miden_client::utils::{Deserializable, Serializable};
use rusqlite::{Connection, Transaction, params};

use super::SqliteStore;
use crate::forest::{ScopedAccountForest, SqliteForestBackend};
use crate::note::apply_note_updates_tx;
use crate::sql_error::SqlResultExt;
use crate::transaction::upsert_transaction_record;
use crate::{insert_sql, subst, with_immediate_write_tx, with_write_tx};

impl SqliteStore {
    pub(crate) fn get_note_tags(conn: &mut Connection) -> Result<Vec<NoteTagRecord>, StoreError> {
        const QUERY: &str = "SELECT tag, source FROM tags";

        conn.prepare_cached(QUERY)
            .into_store_error()?
            .query_map([], |row| Ok((row.get("tag")?, row.get("source")?)))
            .expect("no binding parameters used in query")
            .map(|result| {
                let (tag, source): (Vec<u8>, Vec<u8>) = result.into_store_error()?;
                Ok(NoteTagRecord {
                    tag: NoteTag::read_from_bytes(&tag)
                        .map_err(StoreError::DataDeserializationError)?,
                    source: NoteTagSource::read_from_bytes(&source)
                        .map_err(StoreError::DataDeserializationError)?,
                })
            })
            .collect::<Result<Vec<NoteTagRecord>, _>>()
    }

    pub(crate) fn get_unique_note_tags(
        conn: &mut Connection,
    ) -> Result<BTreeSet<NoteTag>, StoreError> {
        const QUERY: &str = "SELECT DISTINCT tag FROM tags";

        conn.prepare_cached(QUERY)
            .into_store_error()?
            .query_map([], |row| row.get(0))
            .expect("no binding parameters used in query")
            .map(|result| {
                let tag: Vec<u8> = result.into_store_error()?;
                NoteTag::read_from_bytes(&tag).map_err(StoreError::DataDeserializationError)
            })
            .collect::<Result<BTreeSet<NoteTag>, _>>()
    }

    pub(super) fn add_note_tag(
        conn: &mut Connection,
        tag: NoteTagRecord,
    ) -> Result<bool, StoreError> {
        with_write_tx(conn, |tx| add_note_tag_tx(tx, &tag))
    }

    pub(super) fn remove_note_tag(
        conn: &mut Connection,
        tag: NoteTagRecord,
    ) -> Result<usize, StoreError> {
        with_write_tx(conn, |tx| remove_note_tag_tx(tx, tag))
    }

    pub(super) fn get_sync_height(conn: &mut Connection) -> Result<BlockNumber, StoreError> {
        const QUERY: &str = "SELECT block_num FROM blockchain_checkpoint";

        conn.prepare_cached(QUERY)
            .into_store_error()?
            .query_map([], |row| row.get(0))
            .expect("no binding parameters used in query")
            .map(|result| {
                let v: i64 = result.into_store_error()?;
                Ok(BlockNumber::from(u32::try_from(v)?))
            })
            .next()
            .unwrap_or_else(|| {
                Err(StoreError::QueryError("the blockchain checkpoint row is missing".to_string()))
            })
    }

    pub(super) fn apply_state_sync(
        conn: &mut Connection,
        state_sync_update: StateSyncUpdate,
    ) -> Result<(), StoreError> {
        let (
            block_num,
            partial_blockchain_updates,
            note_updates,
            transaction_updates,
            account_updates,
        ) = state_sync_update.into_parts();

        with_immediate_write_tx(conn, |db_tx| {
            let mut smt_forest = ScopedAccountForest::new(SqliteForestBackend::new(db_tx))?;
            // Update blockchain checkpoint (block number and peaks) only if moving forward.
            let new_peaks_bytes = partial_blockchain_updates.new_peaks.peaks().to_vec().to_bytes();
            const BLOCKCHAIN_CHECKPOINT_QUERY: &str = "UPDATE blockchain_checkpoint SET block_num = ?, partial_blockchain_peaks = ? WHERE block_num < ?";
            db_tx
                .execute(
                    BLOCKCHAIN_CHECKPOINT_QUERY,
                    params![
                        i64::from(block_num.as_u32()),
                        new_peaks_bytes,
                        i64::from(block_num.as_u32())
                    ],
                )
                .into_store_error()?;

            for (block_header, is_relevant) in
                partial_blockchain_updates.block_headers_to_store(block_num)
            {
                Self::insert_block_header_tx(db_tx, block_header, *is_relevant)?;
            }

            // Insert new authentication nodes (inner nodes of the PartialBlockchain)
            Self::insert_partial_blockchain_nodes_tx(
                db_tx,
                partial_blockchain_updates.new_authentication_nodes(),
            )?;

            // Update notes
            apply_note_updates_tx(db_tx, &note_updates)?;

            // Remove tags of input notes whose inclusion settled in this sync (committed,
            // consumed during catch-up, or invalidated): their tag no longer drives note sync.
            // Metadata-less records are skipped; their tag (if any) cannot be reconstructed.
            let tags_to_remove = note_updates
                .updated_input_notes()
                .filter_map(|note_update| {
                    let note = note_update.inner();
                    if note.is_inclusion_pending() {
                        None
                    } else {
                        Some(NoteTagRecord {
                            tag: note.metadata()?.tag(),
                            source: NoteTagSource::Note(note.details_commitment()),
                        })
                    }
                })
                .collect::<Vec<_>>();

            for tag in tags_to_remove {
                remove_note_tag_tx(db_tx, tag)?;
            }

            for transaction_record in transaction_updates
                .committed_transactions()
                .chain(transaction_updates.discarded_transactions())
            {
                upsert_transaction_record(db_tx, transaction_record)?;
            }

            // Remove the accounts that are originated from the discarded transactions
            let discarded_states: Vec<(AccountId, Word)> = transaction_updates
                .discarded_transactions()
                .map(|tx| (tx.details.account_id, tx.details.final_account_state))
                .collect();

            Self::undo_account_state(db_tx, &mut smt_forest, &discarded_states)?;

            // Update public accounts on the db that have been updated onchain
            for update in account_updates.updated_public_accounts() {
                match update {
                    PublicAccountUpdate::Full(account) => {
                        Self::update_account_state(db_tx, &mut smt_forest, account)?;
                    },
                    PublicAccountUpdate::Patch { new_header, patch } => {
                        Self::apply_sync_account_patch(db_tx, &mut smt_forest, new_header, patch)?;
                    },
                }
            }

            for (account_id, digest) in account_updates.mismatched_private_accounts() {
                Self::lock_account_on_unexpected_commitment(db_tx, account_id, digest)?;
            }

            Ok(())
        })
    }
}

/// Inserts the tag record, relying on the `(tag, source)` primary key for idempotency across
/// concurrent connections. Returns whether a new row was inserted.
pub(super) fn add_note_tag_tx(
    tx: &Transaction<'_>,
    tag: &NoteTagRecord,
) -> Result<bool, StoreError> {
    const QUERY: &str = insert_sql!(tags { tag, source } | IGNORE);
    let inserted = tx
        .execute(QUERY, params![tag.tag.to_bytes(), tag.source.to_bytes()])
        .into_store_error()?;

    Ok(inserted > 0)
}

pub(super) fn remove_note_tag_tx(
    tx: &Transaction<'_>,
    tag: NoteTagRecord,
) -> Result<usize, StoreError> {
    const QUERY: &str = "DELETE FROM tags WHERE tag = ? AND source = ?";
    let removed_tags = tx
        .execute(QUERY, params![tag.tag.to_bytes(), tag.source.to_bytes()])
        .into_store_error()?;

    Ok(removed_tags)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use crate::SqliteStore;
    use crate::db_management::migration::SqliteMigrator;

    /// A missing checkpoint row is only reachable through corruption (the initial migration seeds
    /// it); it must surface as an error, not a panic.
    #[test]
    fn get_sync_height_errors_when_checkpoint_is_missing() {
        let mut conn = Connection::open_in_memory().unwrap();
        SqliteMigrator::client().apply(&mut conn).unwrap();
        conn.execute("DELETE FROM blockchain_checkpoint", []).unwrap();

        let err = SqliteStore::get_sync_height(&mut conn).unwrap_err();
        assert!(matches!(err, miden_client::store::StoreError::QueryError(_)));
    }
}

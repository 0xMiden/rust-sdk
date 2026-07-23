#![allow(clippy::items_after_statements)]

use std::collections::BTreeSet;
use std::vec::Vec;

use miden_client::Word;
use miden_client::account::AccountId;
use miden_client::note::{BlockNumber, NoteTag};
use miden_client::store::StoreError;
use miden_client::sync::{NoteTagRecord, NoteTagSource, PublicAccountUpdate, StateSyncUpdate};
use miden_client::utils::{Deserializable, Serializable};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};

use super::SqliteStore;
use crate::forest::{ScopedAccountForest, forest_backend};
use crate::note::apply_note_updates_tx;
use crate::sql_error::SqlResultExt;
use crate::transaction::upsert_transaction_record;
use crate::{insert_sql, subst};

impl SqliteStore {
    pub(crate) fn get_note_tags(conn: &mut Connection) -> Result<Vec<NoteTagRecord>, StoreError> {
        const QUERY: &str = "SELECT tag, source FROM tags";

        conn.prepare_cached(QUERY)
            .into_store_error()?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
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
        if Self::get_note_tags(conn)?.contains(&tag) {
            return Ok(false);
        }

        let tx = conn.transaction().into_store_error()?;
        add_note_tag_tx(&tx, &tag)?;

        tx.commit().into_store_error()?;

        Ok(true)
    }

    pub(super) fn remove_note_tag(
        conn: &mut Connection,
        tag: NoteTagRecord,
    ) -> Result<usize, StoreError> {
        let tx = conn.transaction().into_store_error()?;
        let removed_tags = remove_note_tag_tx(&tx, tag)?;

        tx.commit().into_store_error()?;

        Ok(removed_tags)
    }

    pub(super) fn get_sync_height(conn: &mut Connection) -> Result<BlockNumber, StoreError> {
        const QUERY: &str = "SELECT block_num FROM blockchain_checkpoint";

        conn.prepare_cached(QUERY)
            .into_store_error()?
            .query_map([], |row| row.get(0))
            .expect("no binding parameters used in query")
            .map(|result| {
                let v: i64 = result.into_store_error()?;
                Ok(BlockNumber::from(u32::try_from(v).expect("block number is always positive")))
            })
            .next()
            .expect("state sync block number exists")
    }

    pub(super) fn apply_state_sync(
        conn: &mut Connection,
        state_sync_update: StateSyncUpdate,
    ) -> Result<(), StoreError> {
        let StateSyncUpdate {
            block_num,
            partial_blockchain_updates,
            note_updates,
            transaction_updates,
            account_updates,
        } = state_sync_update;

        let db_tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .into_store_error()?;
        {
            let mut scoped_forest = ScopedAccountForest::new(forest_backend(&db_tx))?;
            let smt_forest = &mut scoped_forest;
            let tx = &db_tx;
            // Update blockchain checkpoint (block number and peaks) only if moving forward.
            let new_peaks_bytes = partial_blockchain_updates.new_peaks.peaks().to_vec().to_bytes();
            const BLOCKCHAIN_CHECKPOINT_QUERY: &str = "UPDATE blockchain_checkpoint SET block_num = ?, partial_blockchain_peaks = ? WHERE block_num < ?";
            tx.execute(
                BLOCKCHAIN_CHECKPOINT_QUERY,
                params![
                    i64::from(block_num.as_u32()),
                    new_peaks_bytes,
                    i64::from(block_num.as_u32())
                ],
            )
            .into_store_error()?;

            for (block_header, block_has_relevant_notes) in
                partial_blockchain_updates.block_headers()
            {
                Self::insert_block_header_tx(tx, block_header, *block_has_relevant_notes)?;
            }

            // Insert new authentication nodes (inner nodes of the PartialBlockchain)
            Self::insert_partial_blockchain_nodes_tx(
                tx,
                partial_blockchain_updates.new_authentication_nodes(),
            )?;

            // Update notes
            apply_note_updates_tx(tx, &note_updates)?;

            // Remove tags
            let tags_to_remove = note_updates
                .updated_input_notes()
                .filter_map(|note_update| {
                    let note = note_update.inner();
                    if note.is_committed() {
                        Some(NoteTagRecord {
                            tag: note
                                .metadata()
                                .expect("Committed notes should have metadata")
                                .tag(),
                            source: NoteTagSource::Note(note.details_commitment()),
                        })
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>();

            for tag in tags_to_remove {
                remove_note_tag_tx(tx, tag)?;
            }

            for transaction_record in transaction_updates
                .committed_transactions()
                .chain(transaction_updates.discarded_transactions())
            {
                upsert_transaction_record(tx, transaction_record)?;
            }

            // Remove the accounts that are originated from the discarded transactions
            let discarded_states: Vec<(AccountId, Word)> = transaction_updates
                .discarded_transactions()
                .map(|tx| (tx.details.account_id, tx.details.final_account_state))
                .collect();

            Self::undo_account_state(tx, smt_forest, &discarded_states)?;

            // Update public accounts on the db that have been updated onchain
            for update in account_updates.updated_public_accounts() {
                match update {
                    PublicAccountUpdate::Full(account) => {
                        Self::update_account_state(tx, smt_forest, account)?;
                    },
                    PublicAccountUpdate::Patch { new_header, patch } => {
                        Self::apply_sync_account_patch(tx, smt_forest, new_header, patch)?;
                    },
                }
            }

            for (account_id, digest) in account_updates.mismatched_private_accounts() {
                Self::lock_account_on_unexpected_commitment(tx, account_id, digest)?;
            }
        }
        db_tx.commit().into_store_error()
    }
}

pub(super) fn add_note_tag_tx(tx: &Transaction<'_>, tag: &NoteTagRecord) -> Result<(), StoreError> {
    const QUERY: &str = insert_sql!(tags { tag, source });
    tx.execute(QUERY, params![tag.tag.to_bytes(), tag.source.to_bytes()])
        .into_store_error()?;

    Ok(())
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

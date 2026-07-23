//! Storage-related database operations for accounts.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::string::ToString;
use std::vec::Vec;

use miden_client::account::{
    AccountId,
    AccountStoragePatch,
    StorageMap,
    StorageMapPatch,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
    StorageSlotType,
};
use miden_client::store::{StoreError, add_storage_map_ops, storage_map_lineage_id};
use miden_client::{Deserializable, EMPTY_WORD, Serializable, Word};
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::smt::SmtForestUpdateBatch;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params};

use crate::forest::{forest_entry_keys, forest_lineage_root};
use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, insert_sql, subst, u64_to_value};

impl SqliteStore {
    // READER METHODS
    // --------------------------------------------------------------------------------------------

    /// Fetches the current root values for storage maps that will be updated by the account delta.
    ///
    /// Only queries the slot values (roots) from the latest storage table, avoiding the need to
    /// load full storage map entries into memory. The `AccountSmtForest` handles the actual
    /// Merkle tree operations.
    pub(crate) fn get_storage_map_roots_for_patch(
        conn: &rusqlite::Connection,
        account_id: AccountId,
        storage_patch: &AccountStoragePatch,
    ) -> Result<BTreeMap<StorageSlotName, Word>, StoreError> {
        let map_slot_names: Vec<Value> = storage_patch
            .maps()
            .map(|(slot_name, _)| Value::Text(slot_name.to_string()))
            .collect();

        if map_slot_names.is_empty() {
            return Ok(BTreeMap::new());
        }

        const QUERY: &str = "SELECT slot_name, slot_value FROM latest_account_storage \
                             WHERE account_id = ? AND slot_name IN rarray(?)";

        conn.prepare(QUERY)
            .into_store_error()?
            .query_map(params![account_id.to_bytes(), Rc::new(map_slot_names)], |row| {
                let name: String = row.get(0)?;
                let value: Vec<u8> = row.get(1)?;
                Ok((name, value))
            })
            .into_store_error()?
            .map(|result| {
                let (name, value) = result.into_store_error()?;
                let slot_name = StorageSlotName::new(name)
                    .map_err(|err| StoreError::ParsingError(err.to_string()))?;
                Ok((slot_name, Word::read_from_bytes(&value)?))
            })
            .collect()
    }

    // MUTATOR/WRITER METHODS
    // --------------------------------------------------------------------------------------------

    /// Inserts storage slots into the latest tables only.
    ///
    /// Historical archival is handled separately by the caller when needed.
    pub(crate) fn insert_storage_slots<'a>(
        tx: &Transaction<'_>,
        account_id: AccountId,
        account_storage: impl Iterator<Item = &'a StorageSlot>,
    ) -> Result<(), StoreError> {
        const LATEST_SLOT_QUERY: &str = insert_sql!(
            latest_account_storage {
                account_id,
                slot_name,
                slot_value,
                slot_type
            } | REPLACE
        );
        const LATEST_MAP_ENTRY_QUERY: &str =
            insert_sql!(latest_storage_map_entries { account_id, slot_name, key, value } | REPLACE);

        let mut latest_slot_stmt = tx.prepare_cached(LATEST_SLOT_QUERY).into_store_error()?;
        let mut latest_map_stmt = tx.prepare_cached(LATEST_MAP_ENTRY_QUERY).into_store_error()?;
        let account_id_bytes = account_id.to_bytes();

        for slot in account_storage {
            let slot_name_str = slot.name().to_string();
            let slot_value_bytes = slot.value().to_bytes();
            let slot_type_val = slot.slot_type() as u8;

            latest_slot_stmt
                .execute(params![
                    &account_id_bytes,
                    &slot_name_str,
                    &slot_value_bytes,
                    slot_type_val
                ])
                .into_store_error()?;

            if let StorageSlotContent::Map(map) = slot.content() {
                for (key, value) in map.entries() {
                    latest_map_stmt
                        .execute(params![
                            &account_id_bytes,
                            &slot_name_str,
                            key.to_bytes(),
                            value.to_bytes(),
                        ])
                        .into_store_error()?;
                }
            }
        }

        Ok(())
    }

    /// Writes only the changed storage slots, archiving old values from latest to historical
    /// before overwriting.
    ///
    /// For each changed slot, the old value is read from latest and archived to historical.
    /// NULL `old_slot_value` means the slot was new. For map entries, the old entry value is
    /// similarly archived before updating latest.
    pub(crate) fn write_storage_patch(
        tx: &Transaction<'_>,
        account_id: AccountId,
        nonce: u64,
        updated_slots: &BTreeMap<StorageSlotName, (Word, StorageSlotType)>,
        storage_patch: &AccountStoragePatch,
    ) -> Result<(), StoreError> {
        const LATEST_SLOT_QUERY: &str = insert_sql!(
            latest_account_storage {
                account_id,
                slot_name,
                slot_value,
                slot_type
            } | REPLACE
        );
        const HISTORICAL_SLOT_QUERY: &str = insert_sql!(
            historical_account_storage {
                account_id,
                replaced_at_nonce,
                slot_name,
                old_slot_value,
                slot_type
            } | REPLACE
        );
        const LATEST_MAP_ENTRY_QUERY: &str =
            insert_sql!(latest_storage_map_entries { account_id, slot_name, key, value } | REPLACE);
        const HISTORICAL_MAP_ENTRY_QUERY: &str = insert_sql!(
            historical_storage_map_entries {
                account_id,
                replaced_at_nonce,
                slot_name,
                key,
                old_value
            } | REPLACE
        );
        const READ_OLD_SLOT: &str =
            "SELECT slot_value FROM latest_account_storage WHERE account_id = ? AND slot_name = ?";

        let mut latest_slot_stmt = tx.prepare_cached(LATEST_SLOT_QUERY).into_store_error()?;
        let mut hist_slot_stmt = tx.prepare_cached(HISTORICAL_SLOT_QUERY).into_store_error()?;
        let mut latest_map_stmt = tx.prepare_cached(LATEST_MAP_ENTRY_QUERY).into_store_error()?;
        let mut hist_map_stmt = tx.prepare_cached(HISTORICAL_MAP_ENTRY_QUERY).into_store_error()?;
        let account_id_bytes = account_id.to_bytes();
        let nonce_val = u64_to_value(nonce);

        // Look up each map slot's patch by name so the write path can honor the patch operation.
        let patch_maps: BTreeMap<&StorageSlotName, &StorageMapPatch> =
            storage_patch.maps().collect();

        for (slot_name, (value, slot_type)) in updated_slots {
            let slot_name_str = slot_name.to_string();
            let slot_value_bytes = value.to_bytes();
            let slot_type_val = *slot_type as u8;

            // Read old slot value from latest (NULL if slot is new)
            let old_slot_value: Option<Vec<u8>> = tx
                .query_row(READ_OLD_SLOT, params![&account_id_bytes, &slot_name_str], |row| {
                    row.get(0)
                })
                .optional()
                .into_store_error()?
                .flatten();

            // Archive old value to historical (NULL old_slot_value = slot was new)
            hist_slot_stmt
                .execute(params![
                    &account_id_bytes,
                    &nonce_val,
                    &slot_name_str,
                    old_slot_value,
                    slot_type_val,
                ])
                .into_store_error()?;

            // Update latest slot
            latest_slot_stmt
                .execute(params![
                    &account_id_bytes,
                    &slot_name_str,
                    &slot_value_bytes,
                    slot_type_val
                ])
                .into_store_error()?;

            if let Some(map_patch) = patch_maps.get(slot_name) {
                Self::write_map_patch(
                    tx,
                    &mut latest_map_stmt,
                    &mut hist_map_stmt,
                    &account_id_bytes,
                    &nonce_val,
                    &slot_name_str,
                    map_patch,
                )?;
            }
        }

        Ok(())
    }

    /// Applies a single map slot's patch to the latest and historical tables.
    ///
    /// - `Update` layers the patch entries onto the existing map, deleting entries whose new value
    ///   is the empty word.
    /// - `Create` and `Remove` discard the map's current contents first: every existing entry is
    ///   archived and removed, then the patch's entries (none, for `Remove`) are written. `Create`
    ///   can target an already-populated slot when merged from a remove/create pair, so it cannot
    ///   assume the slot starts empty.
    fn write_map_patch(
        tx: &Transaction<'_>,
        latest_map_stmt: &mut rusqlite::CachedStatement<'_>,
        hist_map_stmt: &mut rusqlite::CachedStatement<'_>,
        account_id_bytes: &[u8],
        nonce_val: &rusqlite::types::Value,
        slot_name_str: &str,
        map_patch: &StorageMapPatch,
    ) -> Result<(), StoreError> {
        match map_patch {
            StorageMapPatch::Update { entries } => {
                let changed: Vec<(Word, Word)> =
                    entries.as_map().iter().map(|(key, value)| ((*key).into(), *value)).collect();
                Self::write_map_entry_delta(
                    tx,
                    latest_map_stmt,
                    hist_map_stmt,
                    account_id_bytes,
                    nonce_val,
                    slot_name_str,
                    &changed,
                )
            },
            StorageMapPatch::Create { entries } => {
                let new_entries: Vec<(Word, Word)> =
                    entries.as_map().iter().map(|(key, value)| ((*key).into(), *value)).collect();
                Self::replace_map_entries(
                    tx,
                    latest_map_stmt,
                    hist_map_stmt,
                    account_id_bytes,
                    nonce_val,
                    slot_name_str,
                    &new_entries,
                )
            },
            StorageMapPatch::Remove => Self::replace_map_entries(
                tx,
                latest_map_stmt,
                hist_map_stmt,
                account_id_bytes,
                nonce_val,
                slot_name_str,
                &[],
            ),
        }
    }

    /// Replaces all latest entries of a map slot with `new_entries`, archiving every affected key.
    ///
    /// Each key in the union of the slot's current keys and `new_entries` is archived exactly once
    /// with its prior value (NULL if the key is new), so historical rows stay consistent. Entries
    /// whose new value is the empty word are treated as absent.
    fn replace_map_entries(
        tx: &Transaction<'_>,
        latest_map_stmt: &mut rusqlite::CachedStatement<'_>,
        hist_map_stmt: &mut rusqlite::CachedStatement<'_>,
        account_id_bytes: &[u8],
        nonce_val: &rusqlite::types::Value,
        slot_name_str: &str,
        new_entries: &[(Word, Word)],
    ) -> Result<(), StoreError> {
        const READ_ALL_MAP_ENTRIES: &str = "SELECT key, value FROM latest_storage_map_entries WHERE account_id = ? AND slot_name = ?";
        const DELETE_ALL_MAP_ENTRIES: &str =
            "DELETE FROM latest_storage_map_entries WHERE account_id = ? AND slot_name = ?";

        let existing: BTreeMap<Vec<u8>, Vec<u8>> = {
            let mut read_stmt = tx.prepare_cached(READ_ALL_MAP_ENTRIES).into_store_error()?;
            let rows = read_stmt
                .query_map(params![account_id_bytes, slot_name_str], |row| {
                    Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .into_store_error()?;
            rows.collect::<Result<_, _>>().into_store_error()?
        };

        let new_map: BTreeMap<Vec<u8>, Vec<u8>> = new_entries
            .iter()
            .filter(|(_, value)| *value != EMPTY_WORD)
            .map(|(key, value)| (key.to_bytes(), value.to_bytes()))
            .collect();

        // Archive each affected key once, recording the value it held before this nonce.
        let mut affected: BTreeSet<&Vec<u8>> = existing.keys().collect();
        affected.extend(new_map.keys());
        for key_bytes in affected {
            let old_value = existing.get(key_bytes).cloned();
            hist_map_stmt
                .execute(params![account_id_bytes, nonce_val, slot_name_str, key_bytes, old_value])
                .into_store_error()?;
        }

        tx.execute(DELETE_ALL_MAP_ENTRIES, params![account_id_bytes, slot_name_str])
            .into_store_error()?;
        for (key_bytes, value_bytes) in &new_map {
            latest_map_stmt
                .execute(params![account_id_bytes, slot_name_str, key_bytes, value_bytes])
                .into_store_error()?;
        }

        Ok(())
    }

    /// Archives old map entry values to historical and updates latest for each changed entry.
    fn write_map_entry_delta(
        tx: &Transaction<'_>,
        latest_map_stmt: &mut rusqlite::CachedStatement<'_>,
        hist_map_stmt: &mut rusqlite::CachedStatement<'_>,
        account_id_bytes: &[u8],
        nonce_val: &rusqlite::types::Value,
        slot_name_str: &str,
        changed_entries: &[(Word, Word)],
    ) -> Result<(), StoreError> {
        const READ_OLD_MAP_ENTRY: &str = "SELECT value FROM latest_storage_map_entries WHERE account_id = ? AND slot_name = ? AND key = ?";
        const DELETE_LATEST_MAP_ENTRY: &str = "DELETE FROM latest_storage_map_entries WHERE account_id = ? AND slot_name = ? AND key = ?";

        for (key, value) in changed_entries {
            let key_bytes = key.to_bytes();

            // Read old map entry value from latest (NULL if entry is new)
            let old_entry_value: Option<Vec<u8>> = tx
                .query_row(
                    READ_OLD_MAP_ENTRY,
                    params![account_id_bytes, slot_name_str, &key_bytes],
                    |row| row.get(0),
                )
                .optional()
                .into_store_error()?
                .flatten();

            // Archive old value to historical (NULL = entry was new)
            hist_map_stmt
                .execute(params![
                    account_id_bytes,
                    nonce_val,
                    slot_name_str,
                    &key_bytes,
                    old_entry_value,
                ])
                .into_store_error()?;

            // Update latest: delete for removals, replace for updates
            if *value == EMPTY_WORD {
                tx.execute(
                    DELETE_LATEST_MAP_ENTRY,
                    params![account_id_bytes, slot_name_str, &key_bytes],
                )
                .into_store_error()?;
            } else {
                latest_map_stmt
                    .execute(
                        params![account_id_bytes, slot_name_str, &key_bytes, value.to_bytes(),],
                    )
                    .into_store_error()?;
            }
        }

        Ok(())
    }

    /// Adds forest update operations for every map-slot patch, returning the touched slot names.
    ///
    /// `Update` layers its entries onto the lineage's latest tree, after verifying that the
    /// tree's root matches the old root recorded in the account tables (a mismatch means the
    /// forest and the account state diverged). `Create` and `Remove` reset the lineage first,
    /// removing every currently stored key, so the resulting tree reflects only the patch's own
    /// entries, or collapses to the empty root for `Remove`.
    pub(crate) fn add_storage_map_patch_ops(
        tx: &Transaction<'_>,
        account_id: AccountId,
        batch: &mut SmtForestUpdateBatch,
        old_map_roots: &BTreeMap<StorageSlotName, Word>,
        storage_patch: &AccountStoragePatch,
    ) -> Result<Vec<StorageSlotName>, StoreError> {
        let default_map_root = StorageMap::default().root();
        let mut touched = Vec::new();

        for (slot_name, map_patch) in storage_patch.maps() {
            touched.push(slot_name.clone());
            let lineage = storage_map_lineage_id(account_id, slot_name);

            match map_patch {
                StorageMapPatch::Update { .. } => {
                    // A lineage the forest does not know yet starts from the empty tree, which
                    // is consistent with an absent old root.
                    let forest_root = forest_lineage_root(tx, lineage)?.unwrap_or(default_map_root);
                    let expected_root =
                        old_map_roots.get(slot_name).copied().unwrap_or(default_map_root);
                    if forest_root != expected_root {
                        return Err(StoreError::MerkleStoreError(MerkleError::ConflictingRoots {
                            expected_root,
                            actual_root: forest_root,
                        }));
                    }
                },
                StorageMapPatch::Create { .. } | StorageMapPatch::Remove => {
                    // Keys re-inserted by the patch below win over these removals (the batch
                    // keeps the last operation per key).
                    for key in forest_entry_keys(tx, lineage)? {
                        batch.operations(lineage).add_remove(key);
                    }
                },
            }

            let entries = map_patch
                .entries()
                .into_iter()
                .flat_map(|e| e.as_map().iter())
                .map(|(key, value)| (*key, *value));
            add_storage_map_ops(batch, account_id, slot_name, entries);
        }

        Ok(touched)
    }
}

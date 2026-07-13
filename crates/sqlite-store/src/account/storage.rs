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
use miden_client::store::{AccountSmtForest, StoreError};
use miden_client::{EMPTY_WORD, Serializable, Word};
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction, params};

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
                let value: String = row.get(1)?;
                Ok((name, value))
            })
            .into_store_error()?
            .map(|result| {
                let (name, value) = result.into_store_error()?;
                let slot_name = StorageSlotName::new(name)
                    .map_err(|err| StoreError::ParsingError(err.to_string()))?;
                Ok((slot_name, Word::try_from(value)?))
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
            let slot_value_hex = slot.value().to_hex();
            let slot_type_val = slot.slot_type() as u8;

            latest_slot_stmt
                .execute(params![&account_id_bytes, &slot_name_str, &slot_value_hex, slot_type_val])
                .into_store_error()?;

            if let StorageSlotContent::Map(map) = slot.content() {
                for (key, value) in map.entries() {
                    latest_map_stmt
                        .execute(params![
                            &account_id_bytes,
                            &slot_name_str,
                            key.to_hex(),
                            value.to_hex(),
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
            let slot_value_hex = value.to_hex();
            let slot_type_val = *slot_type as u8;

            // Read old slot value from latest (NULL if slot is new)
            let old_slot_value: Option<String> = tx
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
                .execute(params![&account_id_bytes, &slot_name_str, &slot_value_hex, slot_type_val])
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

        let existing: BTreeMap<String, String> = {
            let mut read_stmt = tx.prepare_cached(READ_ALL_MAP_ENTRIES).into_store_error()?;
            let rows = read_stmt
                .query_map(params![account_id_bytes, slot_name_str], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .into_store_error()?;
            rows.collect::<Result<_, _>>().into_store_error()?
        };

        let new_map: BTreeMap<String, String> = new_entries
            .iter()
            .filter(|(_, value)| *value != EMPTY_WORD)
            .map(|(key, value)| (key.to_hex(), value.to_hex()))
            .collect();

        // Archive each affected key once, recording the value it held before this nonce.
        let mut affected: BTreeSet<&String> = existing.keys().collect();
        affected.extend(new_map.keys());
        for key_hex in affected {
            let old_value = existing.get(key_hex).cloned();
            hist_map_stmt
                .execute(params![account_id_bytes, nonce_val, slot_name_str, key_hex, old_value])
                .into_store_error()?;
        }

        tx.execute(DELETE_ALL_MAP_ENTRIES, params![account_id_bytes, slot_name_str])
            .into_store_error()?;
        for (key_hex, value_hex) in &new_map {
            latest_map_stmt
                .execute(params![account_id_bytes, slot_name_str, key_hex, value_hex])
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
            let key_hex = key.to_hex();

            // Read old map entry value from latest (NULL if entry is new)
            let old_entry_value: Option<String> = tx
                .query_row(
                    READ_OLD_MAP_ENTRY,
                    params![account_id_bytes, slot_name_str, &key_hex],
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
                    &key_hex,
                    old_entry_value,
                ])
                .into_store_error()?;

            // Update latest: delete for removals, replace for updates
            if *value == EMPTY_WORD {
                tx.execute(
                    DELETE_LATEST_MAP_ENTRY,
                    params![account_id_bytes, slot_name_str, &key_hex],
                )
                .into_store_error()?;
            } else {
                latest_map_stmt
                    .execute(params![account_id_bytes, slot_name_str, &key_hex, value.to_hex(),])
                    .into_store_error()?;
            }
        }

        Ok(())
    }

    /// Applies storage delta changes to the account state, computing new roots via the SMT forest.
    ///
    /// Value-type slot updates are taken directly from the delta. For map-type slots, the old
    /// root is used to update the SMT forest with the delta entries, producing the new root.
    /// Full storage maps are never loaded into memory — the `AccountSmtForest` handles all
    /// Merkle tree operations.
    pub(crate) fn apply_account_storage_patch(
        smt_forest: &mut AccountSmtForest,
        old_map_roots: &BTreeMap<StorageSlotName, Word>,
        storage_patch: &AccountStoragePatch,
    ) -> Result<BTreeMap<StorageSlotName, (Word, StorageSlotType)>, StoreError> {
        let mut updated_slots: BTreeMap<StorageSlotName, (Word, StorageSlotType)> = storage_patch
            .values()
            .map(|(slot_name, value_patch)| {
                (
                    slot_name.clone(),
                    (value_patch.value().unwrap_or_default(), StorageSlotType::Value),
                )
            })
            .collect();

        let default_map_root = StorageMap::default().root();

        for (slot_name, map_patch) in storage_patch.maps() {
            // Update layers its entries onto the existing map. Create and Remove start from an
            // empty map, so Create reflects only its own entries and Remove collapses
            // to the empty root.
            let base_root = match map_patch {
                StorageMapPatch::Update { .. } => {
                    old_map_roots.get(slot_name).copied().unwrap_or(default_map_root)
                },
                StorageMapPatch::Create { .. } | StorageMapPatch::Remove => default_map_root,
            };
            let entries: Vec<_> = map_patch
                .entries()
                .into_iter()
                .flat_map(|e| e.as_map().iter())
                .map(|(key, value)| (*key, *value))
                .collect();

            let new_root = smt_forest.update_storage_map_nodes(base_root, entries.into_iter())?;
            updated_slots.insert(slot_name.clone(), (new_root, StorageSlotType::Map));
        }

        Ok(updated_slots)
    }
}

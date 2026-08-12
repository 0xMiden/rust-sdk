//! Storage-related database operations for accounts.

use std::collections::{BTreeMap, BTreeSet};
use std::string::ToString;
use std::vec::Vec;

use miden_client::account::{
    AccountId,
    AccountStoragePatch,
    StorageMapPatch,
    StorageSlot,
    StorageSlotContent,
    StorageSlotType,
};
use miden_client::store::StoreError;
use miden_client::{EMPTY_WORD, Serializable, Word};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::forest::ScopedAccountForest;
use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, insert_sql, subst, u64_to_value};

impl SqliteStore {
    // READER METHODS
    // --------------------------------------------------------------------------------------------

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
    /// before replacing or removing them.
    ///
    /// The storage patch is the source of truth for slot type, value, and removal. Roots for map
    /// slots that remain present are read from the already-updated forest.
    pub(crate) fn write_storage_patch(
        tx: &Transaction<'_>,
        smt_forest: &ScopedAccountForest<'_, '_>,
        account_id: AccountId,
        nonce: u64,
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
        const DELETE_LATEST_SLOT: &str =
            "DELETE FROM latest_account_storage WHERE account_id = ? AND slot_name = ?";

        let mut latest_slot_stmt = tx.prepare_cached(LATEST_SLOT_QUERY).into_store_error()?;
        let mut hist_slot_stmt = tx.prepare_cached(HISTORICAL_SLOT_QUERY).into_store_error()?;
        let mut latest_map_stmt = tx.prepare_cached(LATEST_MAP_ENTRY_QUERY).into_store_error()?;
        let mut hist_map_stmt = tx.prepare_cached(HISTORICAL_MAP_ENTRY_QUERY).into_store_error()?;
        let account_id_bytes = account_id.to_bytes();
        let nonce_val = u64_to_value(nonce);

        let value_slots = storage_patch.values().map(|(slot_name, value_patch)| {
            Ok::<_, StoreError>((slot_name, value_patch.value(), StorageSlotType::Value, None))
        });
        let map_slots = storage_patch.maps().map(|(slot_name, map_patch)| {
            let new_value = match map_patch {
                StorageMapPatch::Remove => None,
                StorageMapPatch::Create { .. } | StorageMapPatch::Update { .. } => Some(
                    smt_forest
                        .map_root(account_id, slot_name)
                        .ok_or(StoreError::AccountDataNotFound(account_id))?,
                ),
            };
            Ok((slot_name, new_value, StorageSlotType::Map, Some(map_patch)))
        });

        for slot_update in value_slots.chain(map_slots) {
            let (slot_name, new_value, slot_type, map_patch) = slot_update?;
            let slot_name_str = slot_name.to_string();
            let slot_type_val = slot_type as u8;

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

            if let Some(value) = new_value {
                latest_slot_stmt
                    .execute(params![
                        &account_id_bytes,
                        &slot_name_str,
                        value.to_bytes(),
                        slot_type_val
                    ])
                    .into_store_error()?;
            } else {
                tx.execute(DELETE_LATEST_SLOT, params![&account_id_bytes, &slot_name_str])
                    .into_store_error()?;
            }

            if let Some(map_patch) = map_patch {
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
}

//! Storage-related database operations for accounts.

use std::collections::BTreeMap;
use std::string::ToString;
use std::vec::Vec;

use miden_client::account::{
    AccountId,
    AccountStorage,
    AccountStoragePatch,
    StorageMapPatch,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
    StorageSlotPatch,
    StorageSlotType,
    StorageValuePatch,
};
use miden_client::store::StoreError;
use miden_client::{Deserializable, EMPTY_WORD, Serializable, Word};
use rusqlite::{OptionalExtension, Transaction, params};

use crate::account::rows::query_storage_values;
use crate::forest::ScopedAccountForest;
use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, insert_sql, subst, u64_to_value};

impl SqliteStore {
    // READER METHODS
    // --------------------------------------------------------------------------------------------

    // MUTATOR/WRITER METHODS
    // --------------------------------------------------------------------------------------------

    /// Builds the storage patch that takes the stored storage of an account to `storage`.
    ///
    /// Every slot of `storage` becomes a `Create` patch, which the patch writer applies as a
    /// replacement of the slot. Stored slots that `storage` does not have become `Remove` patches.
    pub(crate) fn full_storage_patch(
        tx: &Transaction<'_>,
        account_id: AccountId,
        storage: &AccountStorage,
    ) -> Result<AccountStoragePatch, StoreError> {
        let mut slot_patches: BTreeMap<StorageSlotName, StorageSlotPatch> =
            query_storage_values(tx, account_id)?
                .into_iter()
                .map(|(slot_name, (slot_type, _))| {
                    let removal = match slot_type {
                        StorageSlotType::Value => {
                            StorageSlotPatch::Value(StorageValuePatch::Remove)
                        },
                        StorageSlotType::Map => StorageSlotPatch::Map(StorageMapPatch::Remove),
                    };
                    (slot_name, removal)
                })
                .collect();
        for slot in storage.slots() {
            slot_patches
                .insert(slot.name().clone(), StorageSlotPatch::from(slot.content().clone()));
        }

        AccountStoragePatch::from_raw(slot_patches).map_err(StoreError::AccountPatchError)
    }

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

    /// Writes only the changed storage slots, archiving old values from latest to historical before
    /// replacing or removing them.
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

        let mut read_slot_stmt = tx.prepare_cached(READ_OLD_SLOT).into_store_error()?;
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
            let old_slot_value: Option<Vec<u8>> = read_slot_stmt
                .query_row(params![&account_id_bytes, &slot_name_str], |row| row.get(0))
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
        const READ_MAP_KEYS: &str =
            "SELECT key FROM latest_storage_map_entries WHERE account_id = ? AND slot_name = ?";

        // A replacement is the entry delta that removes every stored key and then writes the new
        // entries over it. The empty word is the removal marker of the delta.
        let mut changed: BTreeMap<Word, Word> = {
            let mut read_stmt = tx.prepare_cached(READ_MAP_KEYS).into_store_error()?;
            let rows = read_stmt
                .query_map(params![account_id_bytes, slot_name_str], |row| {
                    row.get::<_, Vec<u8>>("key")
                })
                .into_store_error()?;
            rows.map(|row| Ok((Word::read_from_bytes(&row.into_store_error()?)?, EMPTY_WORD)))
                .collect::<Result<_, StoreError>>()?
        };
        changed.extend(new_entries.iter().filter(|(_, value)| *value != EMPTY_WORD).copied());

        Self::write_map_entry_delta(
            tx,
            latest_map_stmt,
            hist_map_stmt,
            account_id_bytes,
            nonce_val,
            slot_name_str,
            &changed.into_iter().collect::<Vec<_>>(),
        )
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

        let mut read_stmt = tx.prepare_cached(READ_OLD_MAP_ENTRY).into_store_error()?;
        let mut delete_stmt = tx.prepare_cached(DELETE_LATEST_MAP_ENTRY).into_store_error()?;
        for (key, value) in changed_entries {
            let key_bytes = key.to_bytes();

            // Read old map entry value from latest (NULL if entry is new)
            let old_entry_value: Option<Vec<u8>> = read_stmt
                .query_row(params![account_id_bytes, slot_name_str, &key_bytes], |row| row.get(0))
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
                delete_stmt
                    .execute(params![account_id_bytes, slot_name_str, &key_bytes])
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

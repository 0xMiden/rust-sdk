use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use miden_client_core::sync::StateSyncUpdate;
use miden_protocol::account::{
    AccountHeader,
    AccountStoragePatch,
    StorageMapPatch,
    StorageMapPatchEntries,
    StorageSlotName,
    StorageSlotPatch,
    StorageValuePatch,
};
use miden_protocol::errors::AccountPatchError;
use miden_protocol::note::NoteId;
use miden_protocol::{ONE, Word};

use super::SyncSummary;
use crate::note::NoteUpdateType;
use crate::store::AccountStateUpdate;

impl From<&StateSyncUpdate> for SyncSummary {
    fn from(value: &StateSyncUpdate) -> Self {
        let new_public_note_ids = value
            .note_updates()
            .input_notes()
            .iter()
            .filter_map(|note_update| {
                let note = note_update.inner();
                if let NoteUpdateType::Insert = note_update.update_type() {
                    note.id()
                } else {
                    None
                }
            })
            .collect();

        let committed_note_ids: BTreeSet<NoteId> = value
            .note_updates()
            .input_notes()
            .iter()
            .filter_map(|note_update| {
                let note = note_update.inner();
                // `InsertCommitted` is a previously-tracked expected note that just committed, so
                // it counts as committed (not as a newly-discovered note) even though it is
                // persisted via a full-row insert.
                if matches!(
                    note_update.update_type(),
                    NoteUpdateType::Update | NoteUpdateType::InsertCommitted
                ) && note.is_committed()
                {
                    note.id()
                } else {
                    None
                }
            })
            .chain(value.note_updates().output_notes().iter().filter_map(|note_update| {
                let note = note_update.inner();
                if let NoteUpdateType::Update = note_update.update_type() {
                    note.is_committed().then_some(note.id())
                } else {
                    None
                }
            }))
            .collect();

        let consumed_note_ids: BTreeSet<NoteId> =
            value.note_updates().consumed_input_note_ids().collect();

        SyncSummary::new(
            value.block_num(),
            new_public_note_ids,
            // Populated by Client::sync_state from the Note Transport Layer fetch.
            Vec::new(),
            committed_note_ids.into_iter().collect(),
            consumed_note_ids.into_iter().collect(),
            value
                .account_updates()
                .updated_public_accounts()
                .iter()
                .map(AccountStateUpdate::id)
                .collect(),
            value
                .account_updates()
                .mismatched_private_accounts()
                .iter()
                .map(|(id, _)| *id)
                .collect(),
            value.transaction_updates().committed_transactions().map(|t| t.id).collect(),
        )
    }
}

// STORAGE PATCH BUILDER
// ================================================================================================

/// Builds the absolute [`AccountStoragePatch`] implied by the updates fetched from the node: the
/// value-slot values, the absolute changed map entries per slot, and the complete entries of the
/// maps the node returned in full.
///
/// The carried updates are already merged to the new absolute value of each changed storage slot
/// and map entry, so the patch is assembled directly from them with no need to load the prior
/// account state.
///
/// `changed_map_entries` become `Update` patches layered onto the local map. `complete_map_entries`
/// become `Create` patches, which the store applies as a full replacement of the slot, so they need
/// no removal information.
///
/// A newly created account (final nonce 1) has no prior slots to update, so every value slot and
/// changed map is emitted as a `Create`.
pub(crate) fn build_storage_patch(
    new_header: &AccountHeader,
    value_slot_updates: Vec<(StorageSlotName, Word)>,
    changed_map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
    complete_map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
) -> Result<AccountStoragePatch, AccountPatchError> {
    let is_new_account = new_header.nonce() == ONE;

    let value_entries = value_slot_updates.into_iter().map(|(slot_name, new_value)| {
        let value_patch = if is_new_account {
            StorageValuePatch::Create { value: new_value }
        } else {
            StorageValuePatch::Update { value: new_value }
        };
        (slot_name, StorageSlotPatch::Value(value_patch))
    });

    let changed_maps = changed_map_entries.into_iter().map(|(slot_name, entries)| {
        let map_patch = if is_new_account {
            StorageMapPatch::Create { entries }
        } else {
            StorageMapPatch::Update { entries }
        };
        (slot_name, StorageSlotPatch::Map(map_patch))
    });

    let complete_maps = complete_map_entries.into_iter().map(|(slot_name, entries)| {
        (slot_name, StorageSlotPatch::Map(StorageMapPatch::Create { entries }))
    });

    AccountStoragePatch::from_entries(value_entries.chain(changed_maps).chain(complete_maps))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::vec;

    use miden_protocol::Felt;
    use miden_protocol::account::{AccountId, StorageMapKey, StorageMapPatchEntries};
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;

    use super::*;

    fn account_id() -> AccountId {
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap()
    }

    fn slot_name(name: &str) -> StorageSlotName {
        StorageSlotName::new(name).unwrap()
    }

    fn word(n: u64) -> Word {
        Word::from([
            Felt::new_unchecked(n),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
        ])
    }

    fn header_with_nonce(nonce: u64) -> AccountHeader {
        AccountHeader::new(
            account_id(),
            Felt::new(nonce).expect("test nonce must be a valid Felt"),
            Word::default(),
            Word::default(),
            Word::default(),
        )
    }

    fn build_patch(
        new_nonce: u64,
        value_slot_updates: Vec<(StorageSlotName, Word)>,
        map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
    ) -> Result<AccountStoragePatch, AccountPatchError> {
        build_storage_patch(
            &header_with_nonce(new_nonce),
            value_slot_updates,
            map_entries,
            BTreeMap::new(),
        )
    }

    /// A map the node returned in full becomes a `Create` patch even for an existing account, so
    /// the store replaces the slot instead of layering the entries onto it.
    #[test]
    fn build_patch_creates_complete_maps_for_existing_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let complete_map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_storage_patch(
            &header_with_nonce(2),
            vec![],
            BTreeMap::new(),
            complete_map_entries,
        )
        .unwrap();

        let (_, map_patch) = patch.maps().next().expect("patch should contain map slot");
        assert!(matches!(map_patch, StorageMapPatch::Create { .. }));
    }

    #[test]
    fn build_patch_empty_payload_is_empty() {
        let patch = build_patch(4, vec![], BTreeMap::new()).unwrap();

        assert!(patch.is_empty());
    }

    #[test]
    fn build_patch_sets_value_slot_absolutely() {
        let value_slot = slot_name("miden::test::value");
        let patch = build_patch(2, vec![(value_slot.clone(), word(2))], BTreeMap::new()).unwrap();

        assert_eq!(patch.updated_value(&value_slot), Some(word(2)));
    }

    #[test]
    fn build_patch_wraps_merged_map_entries() {
        let map_slot = slot_name("miden::test::map");
        let key = StorageMapKey::from_raw(word(42));
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(key, word(300));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_patch(2, vec![], map_entries).unwrap();

        let entries = patch.updated_map(&map_slot).expect("patch should contain map slot");
        assert_eq!(entries.as_map().len(), 1);
        assert_eq!(*entries.as_map().values().next().unwrap(), word(300));
    }

    /// A newly created account (final nonce 1) has no prior slots, so its value slots are emitted
    /// as `Create` rather than `Update`.
    #[test]
    fn build_patch_for_new_account_creates_value_slots() {
        let value_slot = slot_name("miden::test::value");
        let patch = build_patch(1, vec![(value_slot, word(1))], BTreeMap::new()).unwrap();

        let (_, value_patch) = patch.values().next().expect("patch should contain value slot");
        assert!(matches!(value_patch, StorageValuePatch::Create { .. }));
    }

    /// A newly created account (final nonce 1, full-state) emits each map slot as a `Create`, which
    /// the store applies by starting the slot from an empty map.
    #[test]
    fn build_patch_emits_map_create_for_new_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_patch(1, vec![], map_entries).unwrap();

        assert!(patch.created_map(&map_slot).is_some());
    }

    /// An update to an existing account (final nonce > 1) emits map slots as `Update`, never
    /// `Create`, so the sync path never asks the store to re-create a populated map.
    #[test]
    fn build_patch_emits_map_update_for_existing_account() {
        let map_slot = slot_name("miden::test::map");
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(StorageMapKey::from_raw(word(1)), word(100));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_patch(2, vec![], map_entries).unwrap();

        assert!(patch.updated_map(&map_slot).is_some());
    }
}

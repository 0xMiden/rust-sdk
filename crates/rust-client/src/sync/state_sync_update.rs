use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use miden_client_core::sync::{PublicAccountUpdate, StateSyncUpdate};
use miden_protocol::account::{
    AccountCode,
    AccountCodePatch,
    AccountHeader,
    AccountPatch,
    AccountStoragePatch,
    AccountVaultPatch,
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

impl From<&StateSyncUpdate> for SyncSummary {
    fn from(value: &StateSyncUpdate) -> Self {
        let new_public_note_ids = value
            .note_updates()
            .updated_input_notes()
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
            .updated_input_notes()
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
            .chain(value.note_updates().updated_output_notes().filter_map(|note_update| {
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
                .map(PublicAccountUpdate::id)
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

/// Builds the absolute [`AccountPatch`] implied by the updates fetched from the node's incremental
/// endpoints: the value-slot values, the absolute changed map entries per slot, and the absolute
/// vault patch.
///
/// The carried updates are already merged to the new absolute value of each changed storage slot,
/// map entry, and vault asset, so the patch is assembled directly from them with no need to load
/// the prior account state.
///
/// A newly created account (final nonce 1) gets a creation patch: every storage slot is a `Create`
/// operation and the patch carries `code`. An update of an existing account (final nonce > 1) gets
/// `Update` operations. It carries `code` only if the code commitment differs from
/// `local_code_commitment`, which means that the account upgraded its code. The caller must
/// validate `code` against the on-chain code commitment.
pub(crate) fn build_account_patch(
    new_header: &AccountHeader,
    value_slot_updates: Vec<(StorageSlotName, Word)>,
    map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
    vault_patch: AccountVaultPatch,
    code: AccountCode,
    local_code_commitment: Word,
) -> Result<AccountPatch, AccountPatchError> {
    let is_new_account = new_header.nonce() == ONE;

    let value_entries = value_slot_updates.into_iter().map(|(slot_name, new_value)| {
        let value_patch = if is_new_account {
            StorageValuePatch::Create { value: new_value }
        } else {
            StorageValuePatch::Update { value: new_value }
        };
        (slot_name, StorageSlotPatch::Value(value_patch))
    });

    let map_entries = map_entries.into_iter().map(|(slot_name, entries)| {
        let map_patch = if is_new_account {
            StorageMapPatch::Create { entries }
        } else {
            StorageMapPatch::Update { entries }
        };
        (slot_name, StorageSlotPatch::Map(map_patch))
    });

    let storage = AccountStoragePatch::from_entries(value_entries.chain(map_entries))?;

    let carries_code = is_new_account || code.commitment() != local_code_commitment;
    let code = AccountCodePatch::new(carries_code.then_some(code));

    AccountPatch::new(new_header.id(), storage, vault_patch, code, Some(new_header.nonce()))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::vec;

    use miden_protocol::Felt;
    use miden_protocol::account::{AccountCode, AccountId, StorageMapKey, StorageMapPatchEntries};
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
    ) -> Result<AccountPatch, AccountPatchError> {
        build_account_patch(
            &header_with_nonce(new_nonce),
            value_slot_updates,
            map_entries,
            AccountVaultPatch::default(),
            AccountCode::mock(),
            AccountCode::mock().commitment(),
        )
    }

    #[test]
    fn build_patch_empty_payload_carries_only_nonce() {
        let patch = build_patch(4, vec![], BTreeMap::new()).unwrap();

        assert_eq!(patch.final_nonce(), Some(Felt::new_unchecked(4)));
        assert!(patch.storage().is_empty());
        assert!(patch.vault().is_empty());
        assert!(patch.code().is_empty());
    }

    #[test]
    fn build_patch_sets_value_slot_absolutely() {
        let value_slot = slot_name("miden::test::value");
        let patch = build_patch(2, vec![(value_slot.clone(), word(2))], BTreeMap::new()).unwrap();

        assert_eq!(patch.storage().updated_value(&value_slot), Some(word(2)));
    }

    #[test]
    fn build_patch_wraps_merged_map_entries() {
        let map_slot = slot_name("miden::test::map");
        let key = StorageMapKey::from_raw(word(42));
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(key, word(300));
        let map_entries = BTreeMap::from([(map_slot.clone(), entries)]);

        let patch = build_patch(2, vec![], map_entries).unwrap();

        let entries =
            patch.storage().updated_map(&map_slot).expect("patch should contain map slot");
        assert_eq!(entries.as_map().len(), 1);
        assert_eq!(*entries.as_map().values().next().unwrap(), word(300));
    }

    #[test]
    fn build_patch_rejects_zero_nonce() {
        let result = build_patch(0, vec![], BTreeMap::new());
        assert!(result.is_err());
    }

    /// A newly created account (final nonce 1) observed via the oversized sync path yields a
    /// creation patch carrying the supplied code, rather than failing to build.
    #[test]
    fn build_patch_for_new_account_carries_code() {
        let value_slot = slot_name("miden::test::value");
        let patch = build_patch(1, vec![(value_slot, word(1))], BTreeMap::new()).unwrap();

        assert_eq!(patch.code().as_code(), Some(&AccountCode::mock()));
        assert_eq!(patch.final_nonce(), Some(ONE));
        assert!(patch.try_to_new_account().is_ok());
    }

    /// An existing account whose on-chain code commitment differs from the local one upgraded its
    /// code. The patch carries the new code and keeps `Update` operations for storage.
    #[test]
    fn build_patch_for_code_upgrade_carries_new_code() {
        let value_slot = slot_name("miden::test::value");
        let local_code_commitment = word(7);
        assert_ne!(local_code_commitment, AccountCode::mock().commitment());

        let patch = build_account_patch(
            &header_with_nonce(3),
            vec![(value_slot.clone(), word(3))],
            BTreeMap::new(),
            AccountVaultPatch::default(),
            AccountCode::mock(),
            local_code_commitment,
        )
        .unwrap();

        assert_eq!(patch.code().as_code(), Some(&AccountCode::mock()));
        assert_eq!(patch.storage().updated_value(&value_slot), Some(word(3)));
        assert!(patch.try_to_new_account().is_err());
    }

    /// An existing account whose code commitment did not change gets a patch without code.
    #[test]
    fn build_patch_without_code_change_omits_code() {
        let value_slot = slot_name("miden::test::value");
        let patch = build_patch(3, vec![(value_slot, word(3))], BTreeMap::new()).unwrap();

        assert!(patch.code().is_empty());
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

        assert!(patch.storage().created_map(&map_slot).is_some());
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

        assert!(patch.storage().updated_map(&map_slot).is_some());
    }
}

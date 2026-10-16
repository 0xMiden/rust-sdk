//! Defines the storage map entries to request for an account.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use miden_protocol::account::{StorageMapKey, StorageSlotName};
use miden_protocol::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

// ACCOUNT STORAGE REQUIREMENTS
// ================================================================================================

/// Per-slot map data to include in a `/GetAccount` response. Slots absent here are omitted from
/// `map_details` (the storage header still lists every slot).
///
/// - Empty key list: all entries, no proof. An oversize map may come back without its entries.
/// - Non-empty key list: just those keys, covered by one partial SMT.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AccountStorageRequirements(BTreeMap<StorageSlotName, Vec<StorageMapKey>>);

impl AccountStorageRequirements {
    /// Requests the specified keys per slot, covered by one partial SMT per slot. An empty key
    /// iterator for a slot behaves like [`Self::all_entries`].
    ///
    /// Repeated keys within a slot are collapsed, since the node rejects a request that names the
    /// same key twice.
    pub fn new<'a>(
        slots_and_keys: impl IntoIterator<
            Item = (StorageSlotName, impl IntoIterator<Item = &'a StorageMapKey>),
        >,
    ) -> Self {
        let map = slots_and_keys
            .into_iter()
            .map(|(slot_name, keys_iter)| {
                let mut keys_vec: Vec<StorageMapKey> = Vec::new();
                for key in keys_iter {
                    if !keys_vec.contains(key) {
                        keys_vec.push(*key);
                    }
                }
                (slot_name, keys_vec)
            })
            .collect();

        AccountStorageRequirements(map)
    }

    /// Requests every entry of each given slot, without a proof. Oversize maps come back without
    /// their entries.
    pub fn all_entries(slot_names: &[StorageSlotName]) -> Self {
        AccountStorageRequirements(
            slot_names.iter().map(|name| (name.clone(), Vec::new())).collect(),
        )
    }

    pub fn inner(&self) -> &BTreeMap<StorageSlotName, Vec<StorageMapKey>> {
        &self.0
    }

    /// Returns the keys requested for a given slot, or an empty slice if none were specified.
    pub fn keys_for_slot(&self, slot_name: &StorageSlotName) -> &[StorageMapKey] {
        self.0.get(slot_name).map_or(&[], Vec::as_slice)
    }
}

impl Serializable for AccountStorageRequirements {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write(&self.0);
    }
}

impl Deserializable for AccountStorageRequirements {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        Ok(AccountStorageRequirements(source.read()?))
    }
}

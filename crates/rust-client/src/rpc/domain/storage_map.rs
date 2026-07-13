use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{StorageMapKey, StorageMapPatchEntries, StorageSlotName};
use miden_protocol::block::BlockNumber;

use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::{RpcError, generated as proto};

/// A single storage-map update as reported by the node: the block it occurred in, the affected slot
/// and key, and the new value.
pub(crate) struct StorageMapUpdate {
    pub(crate) block_num: BlockNumber,
    pub(crate) slot_name: StorageSlotName,
    pub(crate) key: StorageMapKey,
    pub(crate) value: Word,
}

// STORAGE MAP INFO
// ================================================================================================

/// The merged result of syncing an account's storage maps over a block range.
///
/// The node reports per-block map entry updates that may repeat a `(slot, key)` across blocks;
/// these are merged per slot into the absolute changed entries (latest block wins per key). Also
/// provides the current chain tip observed while processing the request.
pub struct StorageMapInfo {
    /// Current chain tip.
    pub chain_tip: BlockNumber,
    /// The block number of the last check included in this response.
    pub block_number: BlockNumber,
    /// The absolute changed entries per storage map slot, merged from the per-block updates.
    pub map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries>,
}

// STORAGE MAP INFO CONVERSION
// ================================================================================================

impl TryFrom<proto::rpc::SyncAccountStorageMapsResponse> for StorageMapInfo {
    type Error = RpcError;

    fn try_from(value: proto::rpc::SyncAccountStorageMapsResponse) -> Result<Self, Self::Error> {
        let pagination_info = value.pagination_info.ok_or(
            proto::rpc::SyncAccountStorageMapsResponse::missing_field(stringify!(pagination_info)),
        )?;

        let updates = value
            .updates
            .into_iter()
            .map(storage_map_update_from_proto)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            chain_tip: pagination_info.chain_tip.into(),
            block_number: pagination_info.block_num.into(),
            map_entries: merge_storage_map_updates(updates),
        })
    }
}

// STORAGE MAP UPDATE
// ================================================================================================

/// Converts a single proto storage-map update into a [`StorageMapUpdate`].
fn storage_map_update_from_proto(
    value: proto::rpc::StorageMapUpdate,
) -> Result<StorageMapUpdate, RpcError> {
    let block_num = BlockNumber::from(value.block_num);

    let slot_name = StorageSlotName::new(value.slot_name)
        .map_err(|err| RpcError::InvalidResponse(err.to_string()))?;

    let key: StorageMapKey = value
        .key
        .ok_or(proto::rpc::StorageMapUpdate::missing_field(stringify!(key)))?
        .try_into()?;

    let map_value: Word = value
        .value
        .ok_or(proto::rpc::StorageMapUpdate::missing_field(stringify!(value)))?
        .try_into()?;

    Ok(StorageMapUpdate {
        block_num,
        slot_name,
        key,
        value: map_value,
    })
}

/// Merges per-block map updates into the absolute changed entries per slot.
///
/// The node may report the same `(slot, key)` in more than one block; applying the updates in
/// ascending block order lets the latest block win, with an empty value encoding a cleared entry.
pub(crate) fn merge_storage_map_updates(
    mut updates: Vec<StorageMapUpdate>,
) -> BTreeMap<StorageSlotName, StorageMapPatchEntries> {
    updates.sort_by_key(|update| update.block_num);

    let mut entries_by_slot: BTreeMap<StorageSlotName, StorageMapPatchEntries> = BTreeMap::new();
    for StorageMapUpdate { slot_name, key, value, .. } in updates {
        entries_by_slot.entry(slot_name).or_default().insert(key, value);
    }
    entries_by_slot
}

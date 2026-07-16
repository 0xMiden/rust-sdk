use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{StorageMapKey, StorageMapPatchEntries, StorageSlotName};
use miden_protocol::block::BlockNumber;

use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::{RpcError, generated as proto};

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

        let mut updates = value
            .updates
            .into_iter()
            .map(storage_map_update_from_proto)
            .collect::<Result<Vec<_>, _>>()?;

        // The node may report the same `(slot, key)` in more than one block, folding the updates
        // in ascending block order lets the latest block win, with an empty value encoding a
        // cleared entry.
        updates.sort_by_key(|(block_num, ..)| *block_num);
        let mut map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries> = BTreeMap::new();
        for (_, slot_name, key, value) in updates {
            map_entries.entry(slot_name).or_default().insert(key, value);
        }

        Ok(Self {
            chain_tip: pagination_info.chain_tip.into(),
            block_number: pagination_info.block_num.into(),
            map_entries,
        })
    }
}

// STORAGE MAP UPDATE
// ================================================================================================

/// Converts a single proto storage-map update into its block number, slot name, key, and new value.
fn storage_map_update_from_proto(
    value: proto::rpc::StorageMapUpdate,
) -> Result<(BlockNumber, StorageSlotName, StorageMapKey, Word), RpcError> {
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

    Ok((block_num, slot_name, key, map_value))
}

use alloc::collections::BTreeMap;

use miden_protocol::account::{StorageMapPatchEntries, StorageSlotName};
use miden_protocol::block::BlockNumber;

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

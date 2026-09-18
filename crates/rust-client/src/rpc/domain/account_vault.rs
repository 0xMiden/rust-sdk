use miden_protocol::account::AccountVaultPatch;
use miden_protocol::block::BlockNumber;

// ACCOUNT VAULT INFO
// ================================================================================================

/// The merged result of syncing an account's vault over a block range.
///
/// The node reports per-block asset updates that may repeat a vault key across blocks; these are
/// merged into a single absolute [`AccountVaultPatch`] (latest block wins per key). Also provides
/// the current chain tip observed while processing the request.
pub struct AccountVaultInfo {
    /// Current chain tip.
    pub chain_tip: BlockNumber,
    /// The block number of the last check included in this response.
    pub block_number: BlockNumber,
    /// The absolute vault patch merged from the per-block updates.
    pub vault_patch: AccountVaultPatch,
}

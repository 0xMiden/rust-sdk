use miden_protocol::block::{BlockHeader, BlockNumber, BlockSignatures};
use miden_protocol::crypto::merkle::mmr::MmrDelta;
use miden_protocol::protocol_config::ProtocolConfig;

// SYNC TARGET
// ================================================================================================

/// Finality level to sync the chain MMR to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTarget {
    /// Sync up to the latest committed block (the chain tip).
    CommittedChainTip,
    /// Sync up to the latest proven block, which may be behind the committed tip.
    ProvenChainTip,
}

// CHAIN MMR INFO
// ================================================================================================

/// Represents the result of a `SyncChainMmr` RPC call, with fields converted into domain types.
pub struct ChainMmrInfo {
    /// The block number from which the delta starts (inclusive).
    pub block_from: BlockNumber,
    /// The block number up to which the delta covers (inclusive).
    pub block_to: BlockNumber,
    /// The MMR delta for the requested block range.
    pub mmr_delta: MmrDelta,
    /// The block header at `block_to`.
    pub block_header: BlockHeader,
    /// The protocol configuration active at `block_to`. The node sends it when `block_from` is
    /// genesis, or when `block_from` and `block_to` commit to different configurations.
    pub protocol_config: Option<ProtocolConfig>,
    /// The validator signatures over `block_header`.
    pub block_signatures: BlockSignatures,
}

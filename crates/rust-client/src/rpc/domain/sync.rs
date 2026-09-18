use miden_objects::DecodeMessageExt;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::MmrDelta;

use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::{RpcError, generated as proto};

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

impl From<SyncTarget> for proto::rpc::FinalityLevel {
    fn from(target: SyncTarget) -> Self {
        match target {
            SyncTarget::CommittedChainTip => Self::Committed,
            SyncTarget::ProvenChainTip => Self::Proven,
        }
    }
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
}

impl TryFrom<proto::rpc::SyncChainMmrResponse> for ChainMmrInfo {
    type Error = RpcError;

    fn try_from(value: proto::rpc::SyncChainMmrResponse) -> Result<Self, Self::Error> {
        let block_range = value
            .block_range
            .ok_or(proto::rpc::SyncChainMmrResponse::missing_field(stringify!(block_range)))?;

        let mmr_delta: MmrDelta = value
            .mmr_delta
            .ok_or(proto::rpc::SyncChainMmrResponse::missing_field(stringify!(mmr_delta)))?
            .decode_and_verify()?;

        let block_header: BlockHeader = value
            .block_header
            .ok_or(proto::rpc::SyncChainMmrResponse::missing_field(stringify!(block_header)))?
            .decode_and_build_unchecked()?;

        Ok(Self {
            block_from: block_range.block_from.into(),
            block_to: block_range.block_to.into(),
            mmr_delta,
            block_header,
        })
    }
}

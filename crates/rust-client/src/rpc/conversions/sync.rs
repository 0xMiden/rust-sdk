use miden_objects::DecodeMessageExt;
use miden_protocol::block::BlockHeader;
use miden_protocol::crypto::merkle::mmr::MmrDelta;

use super::MissingFieldHelper;
use crate::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
use crate::rpc::{RpcError, generated as proto};

// SYNC TARGET
// ================================================================================================

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

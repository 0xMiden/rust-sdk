use alloc::string::ToString;
use core::convert::TryInto;

use crate::rpc::RpcError;
use crate::rpc::domain::status::{
    BlockProducerStatusInfo,
    MempoolStatsInfo,
    NetworkNoteStatus,
    NetworkNoteStatusInfo,
    RpcStatusInfo,
};
use crate::rpc::generated::{self as proto};

impl TryFrom<i32> for NetworkNoteStatus {
    type Error = RpcError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        let value: proto::rpc::NetworkNoteStatus = value
            .try_into()
            .map_err(|_| RpcError::ExpectedDataMissing("NetworkNoteStatus".to_string()))?;

        match value {
            proto::rpc::NetworkNoteStatus::Unspecified => {
                Err(RpcError::ExpectedDataMissing("NetworkNoteStatus".to_string()))
            },
            proto::rpc::NetworkNoteStatus::Pending => Ok(NetworkNoteStatus::Pending),
            proto::rpc::NetworkNoteStatus::NullifierInflight => {
                Ok(NetworkNoteStatus::NullifierInflight)
            },
            proto::rpc::NetworkNoteStatus::Discarded => Ok(NetworkNoteStatus::Discarded),
            proto::rpc::NetworkNoteStatus::NullifierCommitted => {
                Ok(NetworkNoteStatus::NullifierCommitted)
            },
        }
    }
}

impl TryFrom<proto::rpc::GetNetworkNoteStatusResponse> for NetworkNoteStatusInfo {
    type Error = RpcError;

    fn try_from(value: proto::rpc::GetNetworkNoteStatusResponse) -> Result<Self, Self::Error> {
        let status = value.status.try_into()?;
        let last_error = value.last_error;
        let attempt_count = value.attempt_count;
        let last_attempt_block_num = value.last_attempt_block_num;

        Ok(NetworkNoteStatusInfo {
            status,
            last_error,
            attempt_count,
            last_attempt_block_num,
        })
    }
}

impl TryFrom<proto::rpc::RpcStatus> for RpcStatusInfo {
    type Error = RpcError;

    fn try_from(value: proto::rpc::RpcStatus) -> Result<Self, Self::Error> {
        let genesis_commitment = value.genesis_commitment.map(TryInto::try_into).transpose()?;
        Ok(Self {
            version: value.version,
            genesis_commitment,
            chain_tip: value.chain_tip,
            block_producer: value.block_producer.map(Into::into),
        })
    }
}

impl From<proto::rpc::BlockProducerStatus> for BlockProducerStatusInfo {
    fn from(value: proto::rpc::BlockProducerStatus) -> Self {
        Self {
            version: value.version,
            status: value.status,
            chain_tip: value.chain_tip,
            mempool_stats: value.mempool_stats.map(Into::into),
        }
    }
}

impl From<proto::rpc::MempoolStats> for MempoolStatsInfo {
    fn from(value: proto::rpc::MempoolStats) -> Self {
        Self {
            unbatched_transactions: value.unbatched_transactions,
            proposed_batches: value.proposed_batches,
            proven_batches: value.proven_batches,
        }
    }
}

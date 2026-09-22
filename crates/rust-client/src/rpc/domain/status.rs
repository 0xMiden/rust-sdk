use alloc::string::{String, ToString};
use core::convert::TryInto;

use miden_protocol::Word;

use crate::rpc::RpcError;
use crate::rpc::generated::{self as proto};

/// Represents node status info with fields converted into domain types.
pub struct RpcStatusInfo {
    pub version: String,
    pub genesis_commitment: Option<Word>,
    pub chain_tip: u32,
    pub block_producer: Option<BlockProducerStatusInfo>,
}

/// Represents block producer status info with fields converted into domain types.
pub struct BlockProducerStatusInfo {
    pub version: String,
    pub status: String,
    pub chain_tip: u32,
    pub mempool_stats: Option<MempoolStatsInfo>,
}

/// Represents mempool stats with fields converted into domain types.
pub struct MempoolStatsInfo {
    pub unbatched_transactions: u64,
    pub proposed_batches: u64,
    pub proven_batches: u64,
}

pub enum NetworkNoteStatus {
    /// The note is awaiting execution or being retried after transient failures.
    Pending,
    /// The note has been consumed by a transaction that was sent to the block producer.
    NullifierInflight,
    /// The note exceeded the maximum retry count and will not be retried.
    Discarded,
    /// The note's consuming transaction has been committed on-chain.
    NullifierCommitted,
}

impl core::fmt::Display for NetworkNoteStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NetworkNoteStatus::Pending => write!(f, "Pending"),
            NetworkNoteStatus::NullifierInflight => write!(f, "NullifierInflight"),
            NetworkNoteStatus::Discarded => write!(f, "Discarded"),
            NetworkNoteStatus::NullifierCommitted => write!(f, "NullifierCommitted"),
        }
    }
}

/// Information about the processing status of a note submitted to the network.
///
/// This is returned by the `GetNetworkNoteStatus` RPC endpoint and provides details about how the
/// node is handling a note, including retry attempts and error diagnostics.
pub struct NetworkNoteStatusInfo {
    /// The current processing status of the note.
    pub status: NetworkNoteStatus,
    /// The error message from the most recent failed processing attempt, if any.
    pub last_error: Option<String>,
    /// The total number of times the node has attempted to process this note.
    pub attempt_count: u32,
    /// The block number at which the last processing attempt occurred, if any.
    pub last_attempt_block_num: Option<u32>,
}

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

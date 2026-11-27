use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::status::{
    BlockProducerStatusInfo,
    MempoolStatsInfo,
    NetworkNoteSponsorship,
    NetworkNoteStatus,
    NetworkNoteStatusInfo,
    RpcStatusInfo,
};
use miden_objects::DecodeMessageExt;

use super::MissingFieldHelper;
use crate::generated as proto;

fn network_note_status_from_proto(value: i32) -> Result<NetworkNoteStatus, RpcError> {
    let value: proto::node::v1::NetworkNoteStatus = value
        .try_into()
        .map_err(|_| RpcError::ExpectedDataMissing("NetworkNoteStatus".to_string()))?;

    match value {
        proto::node::v1::NetworkNoteStatus::Unspecified => {
            Err(RpcError::ExpectedDataMissing("NetworkNoteStatus".to_string()))
        },
        proto::node::v1::NetworkNoteStatus::Pending => Ok(NetworkNoteStatus::Pending),
        proto::node::v1::NetworkNoteStatus::NullifierInflight => {
            Ok(NetworkNoteStatus::NullifierInflight)
        },
        proto::node::v1::NetworkNoteStatus::Discarded => Ok(NetworkNoteStatus::Discarded),
        proto::node::v1::NetworkNoteStatus::NullifierCommitted => {
            Ok(NetworkNoteStatus::NullifierCommitted)
        },
    }
}

impl TryFrom<proto::node::v1::GetNetworkNoteStatusResponse> for NetworkNoteStatusInfo {
    type Error = RpcError;

    fn try_from(value: proto::node::v1::GetNetworkNoteStatusResponse) -> Result<Self, Self::Error> {
        let status = network_note_status_from_proto(value.status)?;
        let last_error = value.last_error;
        let attempt_count = value.attempt_count;
        let last_attempt_block_num = value.last_attempt_block_num;
        let sponsorships = value
            .sponsorships
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(NetworkNoteStatusInfo {
            status,
            last_error,
            attempt_count,
            last_attempt_block_num,
            sponsorships,
        })
    }
}

impl TryFrom<proto::node::v1::NetworkNoteSponsorship> for NetworkNoteSponsorship {
    type Error = RpcError;

    fn try_from(value: proto::node::v1::NetworkNoteSponsorship) -> Result<Self, Self::Error> {
        let note_id = value
            .note_id
            .ok_or(proto::node::v1::NetworkNoteSponsorship::missing_field(stringify!(note_id)))?
            .decode_and_verify()?;
        let fee_asset = value
            .fee_asset
            .ok_or(proto::node::v1::NetworkNoteSponsorship::missing_field(stringify!(fee_asset)))?
            .decode_and_verify()?;

        Ok(NetworkNoteSponsorship {
            note_id,
            fee_asset,
            reclaim_height: value.reclaim_height,
            committed_block_num: value.committed_block_num,
            last_error: value.last_error,
            last_attempt_block_num: value.last_attempt_block_num,
        })
    }
}

impl TryFrom<proto::node::v1::StatusResponse> for RpcStatusInfo {
    type Error = RpcError;

    fn try_from(value: proto::node::v1::StatusResponse) -> Result<Self, Self::Error> {
        let genesis_commitment = value.genesis_commitment.map(TryInto::try_into).transpose()?;
        Ok(Self {
            version: value.version,
            genesis_commitment,
            chain_tip: value.chain_tip,
            block_producer: value.block_producer.map(Into::into),
        })
    }
}

impl From<proto::node::v1::BlockProducerStatus> for BlockProducerStatusInfo {
    fn from(value: proto::node::v1::BlockProducerStatus) -> Self {
        Self {
            version: value.version,
            status: value.status,
            chain_tip: value.chain_tip,
            mempool_stats: value.mempool_stats.map(Into::into),
        }
    }
}

impl From<proto::node::v1::MempoolStats> for MempoolStatsInfo {
    fn from(value: proto::node::v1::MempoolStats) -> Self {
        Self {
            unbatched_transactions: value.unbatched_transactions,
            proposed_batches: value.proposed_batches,
            proven_batches: value.proven_batches,
        }
    }
}

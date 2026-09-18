use miden_objects::DecodeMessageExt;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::mmr::MmrDelta;
use miden_protocol::protocol_config::ProtocolConfig;

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
    /// The protocol configuration active at `block_to`. The node sends it when the client syncs
    /// from genesis, or when the configuration commitment changed between `block_from` and
    /// `block_to`. It is absent when the client already holds the configuration.
    pub protocol_config: Option<ProtocolConfig>,
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

        let protocol_config: Option<ProtocolConfig> =
            value.protocol_config.map(DecodeMessageExt::decode_and_verify).transpose()?;

        // A configuration that does not match this header is unusable: the store keys a
        // configuration by its own commitment, so a lookup for this header would never find it.
        if let Some(config) = &protocol_config {
            let returned = config.to_commitment();
            let expected = block_header.protocol_config_commitment();
            if returned != expected {
                return Err(RpcError::InvalidResponse(format!(
                    "node returned a protocol configuration with commitment {returned} for a block header that commits to {expected}"
                )));
            }
        }

        Ok(Self {
            block_from: block_range.block_from.into(),
            block_to: block_range.block_to.into(),
            mmr_delta,
            block_header,
            protocol_config,
        })
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec::Vec;

    use miden_protocol::Word;
    use miden_protocol::asset::AssetId;
    use miden_protocol::block::{BlockHeader, FeeParameters};
    use miden_protocol::crypto::merkle::mmr::Forest;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2,
    };

    use super::*;

    fn config_for(faucet: u128) -> ProtocolConfig {
        ProtocolConfig::current(AssetId::new_fungible(faucet.try_into().unwrap())).unwrap()
    }

    /// Builds a response whose header commits to `committed`, carrying `returned`.
    fn response(
        committed: &ProtocolConfig,
        returned: Option<&ProtocolConfig>,
    ) -> proto::rpc::SyncChainMmrResponse {
        let mock = BlockHeader::mock(0, None, None, &[]);
        let header = BlockHeader::new(
            Word::empty(),
            0.into(),
            Word::empty(),
            Word::empty(),
            Word::empty(),
            Word::empty(),
            Word::empty(),
            mock.validator_config().clone(),
            FeeParameters::new(0),
            committed.to_commitment(),
            None,
            0,
        );
        proto::rpc::SyncChainMmrResponse {
            block_range: Some(proto::rpc::BlockRange { block_from: 0, block_to: 0 }),
            mmr_delta: Some(
                MmrDelta {
                    forest: Forest::empty(),
                    data: Vec::new(),
                }
                .into(),
            ),
            block_header: Some((&header).into()),
            block_signatures: Vec::new(),
            protocol_config: returned.map(Into::into),
        }
    }

    #[test]
    fn a_protocol_config_must_match_the_header_it_arrives_with() {
        let committed = config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1);
        let other = config_for(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2);
        assert_ne!(committed.to_commitment(), other.to_commitment());

        let matching = ChainMmrInfo::try_from(response(&committed, Some(&committed))).unwrap();
        assert_eq!(matching.protocol_config.unwrap(), committed);

        let absent = ChainMmrInfo::try_from(response(&committed, None)).unwrap();
        assert!(absent.protocol_config.is_none());

        // `InvalidResponse` covers every structural check, so the message has to name both
        // commitments for this test to prove which one rejected the response.
        let Err(RpcError::InvalidResponse(message)) =
            ChainMmrInfo::try_from(response(&committed, Some(&other)))
        else {
            panic!("a mismatched configuration must be rejected as an invalid response");
        };
        assert!(message.contains(&other.to_commitment().to_string()), "{message}");
        assert!(message.contains(&committed.to_commitment().to_string()), "{message}");
    }
}

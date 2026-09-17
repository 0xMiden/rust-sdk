use miden_objects::DecodeMessageExt;
use miden_protocol::block::{BlockHeader, BlockNumber, FeeParameters, ValidatorConfig};
use miden_protocol::protocol_config::NextProtocolConfig;

use super::{canonical_error, wire_message};
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated as proto;

// BLOCK HEADER
// ================================================================================================

impl From<&BlockHeader> for proto::blockchain::BlockHeader {
    fn from(header: &BlockHeader) -> Self {
        wire_message(&miden_objects::proto::blockchain::BlockHeader::from(header))
    }
}

impl From<&FeeParameters> for proto::blockchain::FeeParameters {
    fn from(fee_params: &FeeParameters) -> Self {
        Self {
            verification_base_fee: fee_params.verification_base_fee(),
        }
    }
}

impl From<FeeParameters> for proto::blockchain::FeeParameters {
    fn from(fee_params: FeeParameters) -> Self {
        (&fee_params).into()
    }
}

impl From<BlockHeader> for proto::blockchain::BlockHeader {
    fn from(header: BlockHeader) -> Self {
        (&header).into()
    }
}

impl TryFrom<proto::blockchain::BlockHeader> for BlockHeader {
    type Error = RpcConversionError;
    fn try_from(value: proto::blockchain::BlockHeader) -> Result<Self, Self::Error> {
        let canonical: miden_objects::proto::blockchain::BlockHeader = wire_message(&value);
        // Chain authentication is performed by VerifyingRpcClient.
        canonical.decode_and_build_unchecked().map_err(canonical_error)
    }
}

impl From<proto::blockchain::FeeParameters> for FeeParameters {
    fn from(value: proto::blockchain::FeeParameters) -> Self {
        FeeParameters::new(value.verification_base_fee)
    }
}

// VALIDATOR CONFIG
// ================================================================================================

impl From<&ValidatorConfig> for proto::blockchain::ValidatorConfig {
    fn from(config: &ValidatorConfig) -> Self {
        wire_message(&miden_objects::proto::blockchain::ValidatorConfig::from(config))
    }
}

// NEXT PROTOCOL CONFIG
// ================================================================================================

impl From<&NextProtocolConfig> for proto::blockchain::NextProtocolConfig {
    fn from(config: &NextProtocolConfig) -> Self {
        wire_message(&miden_objects::proto::blockchain::NextProtocolConfig::from(config))
    }
}

// BLOCK NUMBER
// ================================================================================================

impl From<BlockNumber> for proto::blockchain::BlockNumber {
    fn from(value: BlockNumber) -> Self {
        Self { block_num: value.as_u32() }
    }
}

#[cfg(test)]
mod tests {
    use miden_protocol::Word;

    use super::*;

    #[test]
    fn block_header_round_trip_preserves_protocol_upgrade_and_quorum() {
        let header = BlockHeader::mock(5, None, None, &[]);
        let mut wire: proto::blockchain::BlockHeader = (&header).into();
        let (_, validators) = ValidatorConfig::random_with_signers(3);
        wire.validator_config = Some((&validators).into());
        wire.next_protocol_config = Some(proto::blockchain::NextProtocolConfig {
            effective_from: Some(BlockNumber::from(10u32).into()),
            protocol_config: Some(Word::from([1u32, 2, 3, 4]).into()),
        });
        let decoded = BlockHeader::try_from(wire.clone()).unwrap();
        assert_eq!(proto::blockchain::BlockHeader::from(&decoded), wire);
        assert_eq!(decoded.validator_config().keys(), validators.keys());
        assert_eq!(decoded.validator_config().quorum(), validators.quorum());
        assert_ne!(decoded.commitment(), header.commitment());
    }

    #[test]
    fn block_header_rejects_malformed_wire_fields() {
        let header = BlockHeader::mock(5, None, None, &[]);
        let wire: proto::blockchain::BlockHeader = (&header).into();

        // A version the client does not support.
        let mut malformed = wire.clone();
        malformed.version = 256;
        assert!(BlockHeader::try_from(malformed).is_err());

        // A quorum that does not match the validator count.
        let mut malformed = wire.clone();
        malformed.validator_config.as_mut().unwrap().quorum = 0;
        assert!(BlockHeader::try_from(malformed).is_err());

        // An upgrade scheduled at the genesis height.
        let mut malformed = wire;
        malformed.next_protocol_config = Some(proto::blockchain::NextProtocolConfig {
            effective_from: Some(BlockNumber::GENESIS.into()),
            protocol_config: Some(Word::empty().into()),
        });
        assert!(BlockHeader::try_from(malformed).is_err());
    }
}

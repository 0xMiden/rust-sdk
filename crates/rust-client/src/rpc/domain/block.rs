use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::block::{BlockHeader, BlockNumber, FeeParameters, ValidatorConfig};
use miden_protocol::crypto::dsa::ecdsa_k256_keccak;
use miden_protocol::protocol_config::NextProtocolConfig;
use miden_protocol::utils::serde::{Deserializable, Serializable};

use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated as proto;

// BLOCK HEADER
// ================================================================================================

impl From<&BlockHeader> for proto::blockchain::BlockHeader {
    fn from(header: &BlockHeader) -> Self {
        Self {
            version: header.version().into(),
            prev_block_commitment: Some(header.prev_block_commitment().into()),
            block_num: header.block_num().as_u32(),
            chain_commitment: Some(header.chain_commitment().into()),
            account_root: Some(header.account_root().into()),
            nullifier_root: Some(header.nullifier_root().into()),
            note_root: Some(header.note_root().into()),
            tx_commitment: Some(header.tx_commitment().into()),
            validator_config: Some(proto::blockchain::ValidatorConfig {
                keys: header
                    .validator_config()
                    .keys()
                    .iter()
                    .map(|key| proto::blockchain::ValidatorPublicKey {
                        validator_key: key.to_bytes(),
                    })
                    .collect(),
                quorum: header.validator_config().quorum().into(),
            }),
            protocol_config_commitment: Some(header.protocol_config_commitment().into()),
            next_protocol_config: header.next_protocol_config().map(|config| {
                proto::blockchain::NextProtocolConfig {
                    effective_from: config.effective_from().as_u32(),
                    protocol_config: Some(config.protocol_config().into()),
                }
            }),
            fee_parameters: Some(header.fee_parameters().into()),
            timestamp: header.timestamp(),
        }
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
        if value.version != 1 {
            return Err(RpcConversionError::InvalidField(format!(
                "unsupported block header version {}",
                value.version
            )));
        }
        let config = value
            .validator_config
            .ok_or(proto::blockchain::BlockHeader::missing_field("validator_config"))?;
        let keys = config
            .keys
            .into_iter()
            .map(|key| ecdsa_k256_keccak::PublicKey::read_from_bytes(&key.validator_key))
            .collect::<Result<Vec<_>, _>>()?;
        let validator_config = ValidatorConfig::new(keys, config.quorum.try_into()?)
            .map_err(|err| RpcConversionError::InvalidField(err.to_string()))?;
        let next_protocol_config = value
            .next_protocol_config
            .map(|config| {
                let commitment = config
                    .protocol_config
                    .ok_or(proto::blockchain::NextProtocolConfig::missing_field("protocol_config"))?
                    .try_into()?;
                NextProtocolConfig::new(config.effective_from.into(), commitment)
                    .map_err(|err| RpcConversionError::InvalidField(err.to_string()))
            })
            .transpose()?;

        Ok(BlockHeader::new(
            value
                .prev_block_commitment
                .ok_or(proto::blockchain::BlockHeader::missing_field(stringify!(
                    prev_block_commitment
                )))?
                .try_into()?,
            value.block_num.into(),
            value
                .chain_commitment
                .ok_or(proto::blockchain::BlockHeader::missing_field(stringify!(chain_commitment)))?
                .try_into()?,
            value
                .account_root
                .ok_or(proto::blockchain::BlockHeader::missing_field(stringify!(account_root)))?
                .try_into()?,
            value
                .nullifier_root
                .ok_or(proto::blockchain::BlockHeader::missing_field(stringify!(nullifier_root)))?
                .try_into()?,
            value
                .note_root
                .ok_or(proto::blockchain::BlockHeader::missing_field(stringify!(note_root)))?
                .try_into()?,
            value
                .tx_commitment
                .ok_or(proto::blockchain::BlockHeader::missing_field(stringify!(tx_commitment)))?
                .try_into()?,
            validator_config,
            value
                .fee_parameters
                .ok_or(proto::blockchain::BlockHeader::missing_field(stringify!(fee_parameters)))?
                .try_into()?,
            value
                .protocol_config_commitment
                .ok_or(proto::blockchain::BlockHeader::missing_field("protocol_config_commitment"))?
                .try_into()?,
            next_protocol_config,
            value.timestamp,
        ))
    }
}

impl TryFrom<&proto::blockchain::FeeParameters> for FeeParameters {
    type Error = RpcConversionError;

    fn try_from(value: &proto::blockchain::FeeParameters) -> Result<Self, Self::Error> {
        Ok(FeeParameters::new(value.verification_base_fee))
    }
}

impl TryFrom<proto::blockchain::FeeParameters> for FeeParameters {
    type Error = RpcConversionError;

    fn try_from(value: proto::blockchain::FeeParameters) -> Result<Self, Self::Error> {
        FeeParameters::try_from(&value)
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
        wire.validator_config = Some(proto::blockchain::ValidatorConfig {
            keys: validators
                .keys()
                .iter()
                .map(|key| proto::blockchain::ValidatorPublicKey { validator_key: key.to_bytes() })
                .collect(),
            quorum: 2,
        });
        wire.next_protocol_config = Some(proto::blockchain::NextProtocolConfig {
            effective_from: 10,
            protocol_config: Some(Word::from([1u32, 2, 3, 4]).into()),
        });
        let decoded = BlockHeader::try_from(wire.clone()).unwrap();
        assert_eq!(proto::blockchain::BlockHeader::from(&decoded), wire);
        assert_eq!(decoded.validator_config().keys(), validators.keys());
        assert_eq!(decoded.validator_config().quorum(), 2);
        assert_ne!(decoded.commitment(), header.commitment());
    }

    #[test]
    fn block_header_rejects_invalid_version_quorum_and_upgrade_height() {
        let header = BlockHeader::mock(5, None, None, &[]);
        let wire: proto::blockchain::BlockHeader = (&header).into();
        let mut malformed = wire.clone();
        malformed.version = 256;
        assert!(BlockHeader::try_from(malformed).is_err());
        let mut malformed = wire.clone();
        malformed.validator_config.as_mut().unwrap().quorum = 0;
        assert!(BlockHeader::try_from(malformed).is_err());
        let mut malformed = wire;
        malformed.next_protocol_config = Some(proto::blockchain::NextProtocolConfig {
            effective_from: 0,
            protocol_config: Some(Word::empty().into()),
        });
        assert!(BlockHeader::try_from(malformed).is_err());
    }
}

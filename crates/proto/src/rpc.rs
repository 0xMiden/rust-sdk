//! Protobuf conversion of values that the client receives from the node.

use miden_client::rpc::RpcLimits;
use miden_client::rpc::encryption::TransactionEncryptionKey;
use miden_protocol::crypto::dsa::eddsa_25519_sha512::PublicKey;
use miden_protocol::utils::serde::{Deserializable, Serializable};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for RpcLimits {
    type Message = proto::RpcLimits;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            note_ids_limit: self.note_ids_limit,
            nullifiers_limit: self.nullifiers_limit,
            account_ids_limit: self.account_ids_limit,
            note_tags_limit: self.note_tags_limit,
        }
    }

    fn from_proto(limits: Self::Message) -> Result<Self, ProtoDecodeError> {
        // Proto3 decodes an absent field as zero, and a zero limit makes request chunking panic.
        for (name, limit) in [
            ("note IDs", limits.note_ids_limit),
            ("nullifiers", limits.nullifiers_limit),
            ("account IDs", limits.account_ids_limit),
            ("note tags", limits.note_tags_limit),
        ] {
            if limit == 0 {
                return Err(ProtoDecodeError::InvalidValue(format!(
                    "RPC {name} limit must be greater than zero"
                )));
            }
        }

        Ok(Self {
            note_ids_limit: limits.note_ids_limit,
            nullifiers_limit: limits.nullifiers_limit,
            account_ids_limit: limits.account_ids_limit,
            note_tags_limit: limits.note_tags_limit,
        })
    }
}

impl ProtobufValue for TransactionEncryptionKey {
    type Message = proto::TransactionEncryptionKey;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            scheme: self.scheme(),
            key_id: self.key_id().to_vec(),
            public_key: self.public_key().to_bytes(),
            genesis_commitment: Some(self.genesis_commitment().into()),
        }
    }

    fn from_proto(key: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "transaction encryption key";

        let public_key = PublicKey::read_from_bytes(&key.public_key)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
        let genesis_commitment =
            required(key.genesis_commitment, MESSAGE, "genesis commitment")?.try_into()?;
        TransactionEncryptionKey::from_parts(key.scheme, key.key_id, public_key, genesis_commitment)
            .map_err(ProtoDecodeError::InvalidValue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    #[test]
    fn rpc_limits_with_a_zero_limit_are_rejected() {
        let limits = RpcLimits {
            note_ids_limit: 100,
            nullifiers_limit: 100,
            account_ids_limit: 100,
            note_tags_limit: 0,
        };

        let error = decode::<RpcLimits>(&encode(&limits)).unwrap_err();

        assert!(matches!(error, ProtoDecodeError::InvalidValue(_)), "{error}");
        assert!(error.to_string().contains("note tags"), "{error}");
    }
}

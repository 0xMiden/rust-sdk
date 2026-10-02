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

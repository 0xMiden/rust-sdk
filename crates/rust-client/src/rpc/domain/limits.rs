// RPC LIMITS
// ================================================================================================

use alloc::format;
use core::convert::TryFrom;

use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use crate::rpc::RpcEndpoint;
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated::rpc as proto;

/// Key used to store RPC limits in the settings table.
pub(crate) const RPC_LIMITS_STORE_SETTING: &str = "rpc_limits";

const DEFAULT_NOTE_IDS_LIMIT: u32 = 100;
const DEFAULT_NULLIFIERS_LIMIT: u32 = 1000;
const DEFAULT_ACCOUNT_IDS_LIMIT: u32 = 1000;
const DEFAULT_NOTE_TAGS_LIMIT: u32 = 1000;

/// Domain type representing RPC endpoint limits.
///
/// These limits define the maximum number of items that can be sent in a single RPC request.
/// Exceeding these limits will result in the request being rejected by the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcLimits {
    /// Maximum number of note IDs that can be sent in a single `GetNotesById` request.
    pub note_ids_limit: u32,
    /// Maximum number of nullifier prefixes that can be sent in a single `SyncNullifiers` request.
    pub nullifiers_limit: u32,
    /// Maximum number of account IDs that can be sent in a single `SyncTransactions` request.
    pub account_ids_limit: u32,
    /// Maximum number of note tags that can be sent in a single `SyncNotes` request.
    pub note_tags_limit: u32,
}

impl Default for RpcLimits {
    fn default() -> Self {
        Self {
            note_ids_limit: DEFAULT_NOTE_IDS_LIMIT,
            nullifiers_limit: DEFAULT_NULLIFIERS_LIMIT,
            account_ids_limit: DEFAULT_ACCOUNT_IDS_LIMIT,
            note_tags_limit: DEFAULT_NOTE_TAGS_LIMIT,
        }
    }
}

impl Serializable for RpcLimits {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.note_ids_limit.write_into(target);
        self.nullifiers_limit.write_into(target);
        self.account_ids_limit.write_into(target);
        self.note_tags_limit.write_into(target);
    }
}

impl Deserializable for RpcLimits {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        Ok(Self {
            note_ids_limit: u32::read_from(source)?,
            nullifiers_limit: u32::read_from(source)?,
            account_ids_limit: u32::read_from(source)?,
            note_tags_limit: u32::read_from(source)?,
        })
    }
}

/// Extracts a parameter limit from the proto response for a given endpoint and parameter name.
fn get_param(
    proto: &proto::RpcLimits,
    endpoint: RpcEndpoint,
    param: &'static str,
) -> Result<u32, RpcConversionError> {
    let ep = proto.endpoints.get(endpoint.proto_name()).ok_or(
        RpcConversionError::MissingFieldInProtobufRepresentation {
            entity: "RpcLimits",
            field_name: param,
        },
    )?;
    let limit = ep.parameters.get(param).ok_or(
        RpcConversionError::MissingFieldInProtobufRepresentation {
            entity: "RpcLimits",
            field_name: param,
        },
    )?;
    if *limit == 0 {
        return Err(RpcConversionError::InvalidField(format!(
            "{}.{} must be greater than zero",
            endpoint.proto_name(),
            param
        )));
    }

    Ok(*limit)
}

impl TryFrom<proto::RpcLimits> for RpcLimits {
    type Error = RpcConversionError;

    fn try_from(proto: proto::RpcLimits) -> Result<Self, Self::Error> {
        Ok(Self {
            note_ids_limit: get_param(&proto, RpcEndpoint::GetNotesById, "note_id")?,
            nullifiers_limit: get_param(&proto, RpcEndpoint::SyncNullifiers, "nullifier_prefix")?,
            account_ids_limit: get_param(&proto, RpcEndpoint::SyncTransactions, "account_id")?,
            note_tags_limit: get_param(&proto, RpcEndpoint::SyncNotes, "note_tag")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::String;

    use super::*;

    #[test]
    fn rpc_limits_serialization_roundtrip() {
        let original = RpcLimits {
            note_ids_limit: 100,
            nullifiers_limit: 1000,
            account_ids_limit: 1000,
            note_tags_limit: 1000,
        };

        let bytes = original.to_bytes();
        let deserialized = RpcLimits::read_from_bytes(&bytes).expect("deserialization failed");

        assert_eq!(original, deserialized);
    }

    #[test]
    fn rejects_zero_limits_from_rpc_response() {
        let mut proto = proto::RpcLimits::default();

        proto.endpoints.insert(
            RpcEndpoint::GetNotesById.proto_name().into(),
            proto::EndpointLimits {
                parameters: [(String::from("note_id"), 0)].into(),
            },
        );
        proto.endpoints.insert(
            RpcEndpoint::SyncNullifiers.proto_name().into(),
            proto::EndpointLimits {
                parameters: [(String::from("nullifier_prefix"), 1000)].into(),
            },
        );
        proto.endpoints.insert(
            RpcEndpoint::SyncTransactions.proto_name().into(),
            proto::EndpointLimits {
                parameters: [(String::from("account_id"), 1000)].into(),
            },
        );
        proto.endpoints.insert(
            RpcEndpoint::SyncNotes.proto_name().into(),
            proto::EndpointLimits {
                parameters: [(String::from("note_tag"), 1000)].into(),
            },
        );

        let err = RpcLimits::try_from(proto).expect_err("zero limit should be rejected");

        assert!(matches!(err, RpcConversionError::InvalidField(_)), "got {err:?}");
    }
}

use alloc::format;

use crate::rpc::RpcEndpoint;
use crate::rpc::domain::limits::RpcLimits;
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated::rpc as proto;

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

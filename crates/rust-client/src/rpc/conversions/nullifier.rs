use miden_protocol::Word;
use miden_protocol::note::Nullifier;

use super::MissingFieldHelper;
use crate::rpc::domain::nullifier::NullifierUpdate;
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated as proto;

// CONVERSIONS
// ================================================================================================

/// Reads a nullifier off the wire. A free function because both types are foreign, so there can be
/// no `TryFrom` impl.
pub(super) fn nullifier_from_proto(
    value: proto::primitives::Word,
) -> Result<Nullifier, RpcConversionError> {
    let word: Word = value.try_into()?;
    Ok(Nullifier::from_raw(word))
}

impl TryFrom<&proto::rpc::sync_nullifiers_response::NullifierUpdate> for NullifierUpdate {
    type Error = RpcConversionError;

    fn try_from(
        value: &proto::rpc::sync_nullifiers_response::NullifierUpdate,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            nullifier: nullifier_from_proto(value.nullifier.clone().ok_or(
                proto::rpc::sync_nullifiers_response::NullifierUpdate::missing_field(stringify!(
                    nullifier
                )),
            )?)?,
            block_num: value.block_num.into(),
        })
    }
}

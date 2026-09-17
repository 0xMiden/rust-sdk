use miden_protocol::Word;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::Nullifier;

use crate::rpc::domain::MissingFieldHelper;
use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated as proto;

// NULLIFIER UPDATE
// ================================================================================================

/// Represents a note that was consumed in the node at a certain block.
#[derive(Debug, Clone, Eq, PartialOrd, Ord)]
pub struct NullifierUpdate {
    /// The nullifier of the consumed note.
    pub nullifier: Nullifier,
    /// The number of the block in which the note consumption was registered.
    pub block_num: BlockNumber,
}

impl PartialEq for NullifierUpdate {
    fn eq(&self, other: &Self) -> bool {
        self.nullifier == other.nullifier
    }
}

// CONVERSIONS
// ================================================================================================

impl TryFrom<proto::primitives::Word> for Nullifier {
    type Error = RpcConversionError;

    fn try_from(value: proto::primitives::Word) -> Result<Self, Self::Error> {
        let word: Word = value.try_into()?;
        Ok(Self::from_raw(word))
    }
}

impl TryFrom<&proto::rpc::sync_nullifiers_response::NullifierUpdate> for NullifierUpdate {
    type Error = RpcConversionError;

    fn try_from(
        value: &proto::rpc::sync_nullifiers_response::NullifierUpdate,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            nullifier: value
                .nullifier
                .clone()
                .ok_or(proto::rpc::sync_nullifiers_response::NullifierUpdate::missing_field(
                    stringify!(nullifier),
                ))?
                .try_into()?,
            block_num: value.block_num.into(),
        })
    }
}

use miden_protocol::account::StorageMapKey;
use miden_protocol::note::NoteId;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Felt, Word};

use crate::rpc::errors::RpcConversionError;
use crate::rpc::generated as proto;

impl From<Word> for proto::primitives::Word {
    fn from(value: Word) -> Self {
        Self { encoded: value.to_bytes() }
    }
}
impl From<&Word> for proto::primitives::Word {
    fn from(value: &Word) -> Self {
        (*value).into()
    }
}
impl From<NoteId> for proto::primitives::Word {
    fn from(value: NoteId) -> Self {
        value.as_word().into()
    }
}
impl From<&NoteId> for proto::primitives::Word {
    fn from(value: &NoteId) -> Self {
        value.as_word().into()
    }
}
impl TryFrom<&proto::primitives::Word> for Word {
    type Error = RpcConversionError;
    fn try_from(value: &proto::primitives::Word) -> Result<Self, Self::Error> {
        if value.encoded.len() != 32 {
            return Err(RpcConversionError::InvalidField(
                "word must contain exactly 32 bytes".into(),
            ));
        }
        Ok(Word::read_from_bytes(&value.encoded)?)
    }
}
impl TryFrom<proto::primitives::Word> for Word {
    type Error = RpcConversionError;
    fn try_from(value: proto::primitives::Word) -> Result<Self, Self::Error> {
        (&value).try_into()
    }
}
impl TryFrom<proto::primitives::Word> for [Felt; 4] {
    type Error = RpcConversionError;
    fn try_from(value: proto::primitives::Word) -> Result<Self, Self::Error> {
        Ok(Word::try_from(value)?.into())
    }
}
impl TryFrom<&proto::primitives::Word> for [Felt; 4] {
    type Error = RpcConversionError;
    fn try_from(value: &proto::primitives::Word) -> Result<Self, Self::Error> {
        Ok(Word::try_from(value)?.into())
    }
}
impl TryFrom<proto::primitives::Word> for StorageMapKey {
    type Error = RpcConversionError;
    fn try_from(value: proto::primitives::Word) -> Result<Self, Self::Error> {
        Ok(Self::new(value.try_into()?))
    }
}

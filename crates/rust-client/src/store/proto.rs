//! Protobuf bindings for the values the store keeps, generated from `proto/store.proto`.
//!
//! A message field is optional in protobuf, so a value the store requires arrives as `None` when
//! the row was written by a version that did not set it. [`required`] turns that into an error at
//! the point of use, and names the field it was reading.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use miden_objects::{ConversionError, DecodeMessageExt, proto as objects};
use miden_protocol::Word;
use miden_protocol::account::AccountCode;
use miden_protocol::block::BlockHeader;
use miden_protocol::note::{NoteAttachments, NoteMetadata, NoteScript, NoteStorage};
use miden_protocol::transaction::TransactionScript;
use miden_tx::utils::serde::DeserializationError;

#[rustfmt::skip]
#[allow(clippy::doc_markdown, clippy::large_enum_variant, missing_docs)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/store/miden.client.store.rs"));
}
pub use generated::*;

// PROTOBUF VALUE
// ================================================================================================

/// A value that the store keeps as a protobuf message.
pub trait ProtobufValue: Sized {
    /// The message that the store writes for this value.
    type Message: prost::Message + Default;

    /// Builds the message that the store writes.
    fn to_proto(&self) -> Self::Message;

    /// Builds the value from a stored message and checks its domain constraints.
    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError>;
}

/// Encodes a value as the protobuf message that the store keeps.
pub fn encode<T: ProtobufValue>(value: &T) -> Vec<u8> {
    prost::Message::encode_to_vec(&value.to_proto())
}

/// Decodes a stored protobuf message and builds the value from it.
pub fn decode<T: ProtobufValue>(bytes: &[u8]) -> Result<T, ProtoDecodeError> {
    let message = <T::Message as prost::Message>::decode(bytes)?;
    T::from_proto(message)
}

// ERRORS
// ================================================================================================

/// Errors that occur when the store reads a protobuf value.
#[derive(Debug, thiserror::Error)]
pub enum ProtoDecodeError {
    /// The bytes are not a valid protobuf message.
    #[error("invalid protobuf message")]
    Wire(#[from] prost::DecodeError),
    /// A field that the value requires is absent.
    #[error("{message} has no {field}")]
    MissingField {
        message: &'static str,
        field: &'static str,
    },
    /// A protocol message does not describe a valid protocol value.
    #[error("invalid protocol value")]
    Conversion(#[from] ConversionError),
    /// A `bytes` field does not hold a valid `Serializable` encoding.
    #[error("invalid serialized value")]
    Serialized(#[from] DeserializationError),
    /// The message is well formed, but its content breaks a rule of the value.
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

// HELPERS
// ================================================================================================

/// Returns a field that the store requires, or an error that names it.
pub(crate) fn required<T>(
    field: Option<T>,
    message: &'static str,
    name: &'static str,
) -> Result<T, ProtoDecodeError> {
    field.ok_or(ProtoDecodeError::MissingField { message, field: name })
}

// PROTOCOL VALUES
// ================================================================================================

// The store keeps these protocol values in their own columns, as the messages that `miden-objects`
// defines.

impl ProtobufValue for AccountCode {
    type Message = objects::account::AccountCode;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for TransactionScript {
    type Message = objects::transaction::TransactionScript;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteScript {
    type Message = objects::note::NoteScript;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteAttachments {
    type Message = objects::note::NoteAttachments;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteStorage {
    type Message = objects::note::NoteStorage;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteMetadata {
    type Message = objects::note::NoteMetadata;

    fn to_proto(&self) -> Self::Message {
        (*self).into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

/// The store only keeps headers that it received from the node or built from them, so a stored
/// header is not verified again when it is read.
impl ProtobufValue for BlockHeader {
    type Message = objects::blockchain::BlockHeader;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_build_unchecked()?)
    }
}

impl ProtobufValue for miden_protocol::note::NoteAssets {
    type Message = NoteAssets;

    fn to_proto(&self) -> Self::Message {
        NoteAssets {
            assets: self.iter().map(Into::into).collect(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        let assets = message
            .assets
            .into_iter()
            .map(DecodeMessageExt::decode_and_verify)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(assets).map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

/// The peaks of the partial blockchain MMR, without their forest.
impl ProtobufValue for Vec<Word> {
    type Message = MmrPeaks;

    fn to_proto(&self) -> Self::Message {
        MmrPeaks {
            peaks: self.iter().map(Into::into).collect(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.peaks.into_iter().map(Word::try_from).collect::<Result<_, _>>()?)
    }
}

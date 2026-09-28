//! Protobuf format of the values this store keeps, generated from the schemas in `proto/store`.
//!
//! A message field is optional in protobuf, so a value the store requires arrives as `None` when
//! the row was written by a version that did not set it. `required` turns that into an error at the
//! point of use, and names the field it was reading.

use std::string::{String, ToString};
use std::vec::Vec;

use miden_client::store::StoreError;
use miden_client::utils::DeserializationError;
use miden_objects::ConversionError;

mod input_note;
mod output_note;
mod protocol;
mod settings;
mod transaction;

pub use output_note::{decode_output_note_state, encode_output_note_state};

#[rustfmt::skip]
#[allow(clippy::doc_markdown, clippy::large_enum_variant, missing_docs)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/miden.client.store.rs"));
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
    /// An enum field holds a value that the schema does not define.
    #[error(transparent)]
    UnknownEnumValue(#[from] prost::UnknownEnumValue),
    /// A field that the value requires is absent.
    #[error("{message} has no {field}")]
    MissingField {
        message: &'static str,
        field: &'static str,
    },
    /// A protocol message does not describe a valid protocol value.
    #[error("invalid protocol value")]
    Conversion(#[from] ConversionError),
    /// The message is well formed, but its content breaks a rule of the value.
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

/// The client reports a value that cannot be read back as a deserialization error, whatever the
/// store's format.
impl From<ProtoDecodeError> for StoreError {
    fn from(err: ProtoDecodeError) -> Self {
        StoreError::DataDeserializationError(DeserializationError::InvalidValue(err.to_string()))
    }
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

#[cfg(test)]
mod tests {
    use miden_client::note::BlockNumber;
    use miden_client::transaction::TransactionStatus;

    use super::*;

    /// A later client version can add fields to a stored message. A row that carries a field the
    /// reader does not know must still decode to the same value.
    #[test]
    fn decoding_skips_unknown_fields() {
        let status = TransactionStatus::Committed {
            block_number: BlockNumber::from(7u32),
            commit_timestamp: 42,
        };
        let mut bytes = encode(&status);
        // Field 15 as a varint with value 1: the tag byte is (15 << 3) | 0.
        bytes.extend_from_slice(&[15 << 3, 1]);

        assert_eq!(decode::<TransactionStatus>(&bytes).unwrap(), status);
    }
}

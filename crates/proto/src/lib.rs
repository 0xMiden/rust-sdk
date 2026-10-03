//! Protobuf serialization of miden-client types, generated from the schemas in `proto/client`.
//!
//! A message field is optional in protobuf, so a required value arrives as `None` when the message
//! was written by a version that did not set it. `required` turns that into an error at the point
//! of use, and names the field it was reading.
//!
//! A scalar field without `optional` has no presence, so an absent value reads as zero. Declare a
//! new scalar field as `optional` when the reader must detect that it is absent.

use std::string::{String, ToString};
use std::vec::Vec;

use miden_client::store::StoreError;
use miden_client::utils::DeserializationError;
use miden_objects::ConversionError;

mod input_note;
mod output_note;
mod protocol;
mod transaction;

pub use output_note::{
    decode_output_note_state_without_script,
    encode_output_note_state_without_script,
};
pub use protocol::{decode_mmr_peaks, encode_mmr_peaks};

#[rustfmt::skip]
#[allow(
    clippy::doc_markdown,
    clippy::large_enum_variant,
    clippy::struct_field_names,
    clippy::trivially_copy_pass_by_ref,
    missing_docs
)]
mod generated {
    // Each module is one `client.*` package. The modules are siblings, so the `super::` paths that
    // prost generates between packages resolve.
    pub mod input_note {
        include!(concat!(env!("OUT_DIR"), "/client.input_note.rs"));
    }
    pub mod output_note {
        include!(concat!(env!("OUT_DIR"), "/client.output_note.rs"));
    }
    pub mod protocol {
        include!(concat!(env!("OUT_DIR"), "/client.protocol.rs"));
    }
    pub mod transaction {
        include!(concat!(env!("OUT_DIR"), "/client.transaction.rs"));
    }
}
pub use generated::input_note::*;
pub use generated::output_note::*;
pub use generated::protocol::*;
pub use generated::transaction::*;

// PROTOBUF VALUE
// ================================================================================================

/// A value that has a protobuf message format.
pub trait ProtobufValue: Sized {
    /// The message that represents this value.
    type Message: prost::Message + Default;

    /// Builds the message for this value.
    fn to_proto(&self) -> Self::Message;

    /// Builds the value from a message and checks all of its constraints.
    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError>;

    /// Builds the value from a message and may skip the checks that are expensive. A type without
    /// such checks builds the value as `from_proto` does. Use it only for messages from a trusted
    /// source.
    fn from_proto_unchecked(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Self::from_proto(message)
    }
}

/// Encodes a value as its protobuf message.
pub fn encode<T: ProtobufValue>(value: &T) -> Vec<u8> {
    prost::Message::encode_to_vec(&value.to_proto())
}

/// Decodes a protobuf message and builds the value from it with all checks.
pub fn decode<T: ProtobufValue>(bytes: &[u8]) -> Result<T, ProtoDecodeError> {
    let message = <T::Message as prost::Message>::decode(bytes)?;
    T::from_proto(message)
}

/// Decodes a protobuf message and builds the value from it with
/// [`ProtobufValue::from_proto_unchecked`]. Use it only for bytes from a trusted source.
pub fn decode_unchecked<T: ProtobufValue>(bytes: &[u8]) -> Result<T, ProtoDecodeError> {
    let message = <T::Message as prost::Message>::decode(bytes)?;
    T::from_proto_unchecked(message)
}

// ERRORS
// ================================================================================================

/// Errors that occur when a protobuf value is decoded.
#[derive(Debug, thiserror::Error)]
pub enum ProtoDecodeError {
    /// The bytes are not a valid protobuf message.
    #[error("invalid protobuf message: {0}")]
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
    #[error("invalid protocol value: {0}")]
    Conversion(#[from] ConversionError),
    /// The message is well formed, but its content breaks a rule of the value.
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

impl From<ProtoDecodeError> for StoreError {
    fn from(err: ProtoDecodeError) -> Self {
        StoreError::DataDeserializationError(DeserializationError::InvalidValue(err.to_string()))
    }
}

// HELPERS
// ================================================================================================

/// Returns a required field, or an error that names it.
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

    /// A later client version can add fields to a message. A message that carries a field the
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

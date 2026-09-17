use core::any::type_name;

use super::errors::RpcConversionError;

pub mod account;
pub mod account_vault;
pub mod block;
pub mod digest;
pub mod limits;
pub mod logs;
pub mod merkle;
pub mod note;
pub mod nullifier;
pub mod smt;
pub mod status;
pub mod storage_map;
pub mod sync;
pub mod transaction;

pub use logs::{AccountLogCursor, AccountLogPage, AccountLogQuery, AccountLogRecord};

// UTILITIES
// ================================================================================================

pub trait MissingFieldHelper {
    fn missing_field(field_name: &'static str) -> RpcConversionError;
}

impl<T: prost::Message> MissingFieldHelper for T {
    fn missing_field(field_name: &'static str) -> RpcConversionError {
        RpcConversionError::MissingFieldInProtobufRepresentation {
            entity: type_name::<T>(),
            field_name,
        }
    }
}

/// Bridges independently generated bindings for the identical canonical schema. Domain validation
/// is deliberately performed by the caller after this wire-only conversion.
pub(crate) fn wire_message<S: prost::Message, T: prost::Message + Default>(source: &S) -> T {
    T::decode(source.encode_to_vec().as_slice()).expect("identical protobuf schemas")
}

pub(crate) fn canonical_error(error: impl core::fmt::Display) -> RpcConversionError {
    RpcConversionError::InvalidField(alloc::format!("{error}"))
}

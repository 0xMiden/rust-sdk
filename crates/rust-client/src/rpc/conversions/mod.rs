//! Conversions between the generated protobuf messages and the RPC domain types.
//!
//! The types in [`crate::rpc::domain`] do not depend on the wire format. Every conversion to or
//! from a generated message lives here, next to the gRPC client that sends and receives them.

use core::any::type_name;

use crate::rpc::errors::RpcConversionError;

mod account;
mod account_vault;
mod encryption;
mod limits;
mod note;
mod nullifier;
mod status;
mod storage_map;
mod sync;
mod transaction;

// UTILITIES
// ================================================================================================

/// Builds the error for a required protobuf field that is absent.
pub(crate) trait MissingFieldHelper {
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

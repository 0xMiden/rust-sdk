use core::any::type_name;

use super::errors::RpcConversionError;

pub mod account;
pub mod account_vault;
pub mod limits;
pub mod note;
pub mod nullifier;
pub mod status;
pub mod storage_map;
pub mod sync;
pub mod transaction;

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

use core::any::type_name;

use miden_client_core::errors::RpcConversionError;

mod account;
pub(crate) use account::{storage_map_fetch_to_proto, vault_fetch_to_proto};
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

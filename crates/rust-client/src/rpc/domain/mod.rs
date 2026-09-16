use core::any::type_name;

use miden_objects::decoded::VerificationError;
use miden_objects::{BuildUnchecked, DecodeMessage, Verify};

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

/// Decodes a canonical object message and checks the domain invariants of the result.
pub(crate) fn verify_message<M, T>(message: M) -> Result<T, RpcConversionError>
where
    M: DecodeMessage,
    M::Decoded: Verify<Verified = T>,
{
    message
        .decode_fields()?
        .verify()
        .map_err(|err| RpcConversionError::CanonicalVerification(VerificationError::new(err)))
}

/// Decodes a canonical object message and builds the domain value without the checks that need
/// external context.
pub(crate) fn build_unchecked_message<M, T>(message: M) -> Result<T, RpcConversionError>
where
    M: DecodeMessage,
    M::Decoded: BuildUnchecked<Output = T>,
{
    message
        .decode_fields()?
        .build_unchecked()
        .map_err(|err| RpcConversionError::CanonicalVerification(VerificationError::new(err)))
}

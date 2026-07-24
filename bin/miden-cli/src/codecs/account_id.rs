//! The `account-id` codec for typed `call` rendering.
//!
//! `account-id` felts are validated with protocol-level rules, so the CLI registers this codec
//! (via [`TypedProcInfo::with_scalar_codec`]) to encode one hex token into the two stack felts the
//! procedure expects and render the returned felts back as `account-id(0x..)`.
//!
//! [`TypedProcInfo::with_scalar_codec`]: miden_mast_package::typed::TypedProcInfo::with_scalar_codec

use miden_mast_package::typed::{MIDEN_CORE_TYPES, TypedError, WitScalarCodec};
use miden_protocol::Felt;
use miden_protocol::account::AccountId;

use crate::codecs::invalid_scalar;

/// Bare WIT type name the typed encoder matches this codec against, regardless of the package and
/// version in the full type name (e.g. `miden:base/core-types@1.0.0/account-id`).
const ACCOUNT_ID_WIT_NAME: &str = "account-id";

/// Encodes and renders the WIT `account-id` type: one hex token, two stack felts.
pub struct AccountIdCodec;

impl WitScalarCodec for AccountIdCodec {
    fn wit_name(&self) -> &str {
        ACCOUNT_ID_WIT_NAME
    }

    fn wit_interface(&self) -> Option<&str> {
        Some(MIDEN_CORE_TYPES)
    }

    fn encode(&self, token: &str) -> Result<Vec<Felt>, TypedError> {
        let id = AccountId::from_hex(token)
            .map_err(|err| invalid_scalar(ACCOUNT_ID_WIT_NAME, token, &err))?;
        let [prefix, suffix]: [Felt; 2] = id.into();
        Ok(vec![prefix, suffix])
    }

    fn decode(&self, felts: &[Felt]) -> Result<String, TypedError> {
        // The caller passes as many felts as the type occupies, so any other count means the
        // signature and this codec disagree about the value's width.
        let [prefix, suffix] = felts else {
            return Err(TypedError::MalformedResult {
                ty: ACCOUNT_ID_WIT_NAME.to_string(),
                reason: "an account id occupies exactly two felts",
            });
        };
        let id = AccountId::try_from_elements(*suffix, *prefix).map_err(|_| {
            TypedError::MalformedResult {
                ty: ACCOUNT_ID_WIT_NAME.to_string(),
                reason: "the felts are not a valid account id",
            }
        })?;
        Ok(format!("account-id({})", id.to_hex()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_one_hex_token_roundtrips() {
        let codec = AccountIdCodec;
        let hex = "0xaa0000000000bb110000cc000000dd";

        // Compared against the felts the account id itself carries: a round-trip alone would also
        // pass if `encode` and `decode` had the two fields the same way around.
        let expected: [Felt; 2] = AccountId::from_hex(hex).unwrap().into();
        assert_eq!(codec.encode(hex).unwrap(), expected);

        assert_eq!(codec.decode(&expected).unwrap(), format!("account-id({hex})"));
    }

    #[test]
    fn felts_that_are_not_an_account_id_are_rejected() {
        let err = AccountIdCodec.decode(&[Felt::from(1u32), Felt::from(2u32)]).unwrap_err();
        assert!(matches!(err, TypedError::MalformedResult { .. }));
    }

    #[test]
    fn invalid_account_id_token_is_rejected() {
        let err = AccountIdCodec.encode("not-hex").unwrap_err();
        assert!(matches!(err, TypedError::InvalidScalar { .. }));
    }
}

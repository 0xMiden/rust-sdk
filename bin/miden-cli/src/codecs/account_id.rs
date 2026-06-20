//! The `account-id` codec for typed `call` rendering.
//!
//! `account-id` felts are validated with protocol-level rules, so the CLI registers this codec
//! (via [`TypedProcInfo::with_scalar_codec`]) to encode one hex token into the two stack felts the
//! procedure expects and render the returned felts back as `account-id(0x..)`.
//!
//! [`TypedProcInfo::with_scalar_codec`]: miden_mast_package::debug_info::typed::TypedProcInfo::with_scalar_codec

use miden_mast_package::debug_info::typed::{TypedDebugInfoError, WitScalarCodec};
use miden_protocol::Felt;
use miden_protocol::account::AccountId;

use crate::codecs::invalid_scalar;

/// Bare WIT type name the typed encoder matches this codec against, regardless of the package and
/// version in the full debug type path (e.g. `miden:base/core-types@1.0.0/account-id`).
const ACCOUNT_ID_WIT_NAME: &str = "account-id";

/// Encodes and renders the WIT `account-id` type: one hex token, two stack felts.
pub struct AccountIdCodec;

impl WitScalarCodec for AccountIdCodec {
    fn wit_name(&self) -> &str {
        ACCOUNT_ID_WIT_NAME
    }

    fn felt_count(&self) -> usize {
        2
    }

    fn encode(&self, token: &str) -> Result<Vec<Felt>, TypedDebugInfoError> {
        let id = AccountId::from_hex(token)
            .map_err(|err| invalid_scalar(ACCOUNT_ID_WIT_NAME, token, &err))?;
        let [prefix, suffix]: [Felt; 2] = id.into();
        Ok(vec![prefix, suffix])
    }

    fn decode(&self, felts: &[Felt]) -> Option<String> {
        let [prefix, suffix, ..] = felts else {
            return None;
        };
        let id = AccountId::try_from_elements(*suffix, *prefix).ok()?;
        Some(format!("account-id({})", id.to_hex()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn account_id_one_hex_token_roundtrips() {
        let codec = AccountIdCodec;
        let hex = "0xaa0000000000bb110000cc000000dd";

        let felts = codec.encode(hex).unwrap();
        assert_eq!(felts.len(), 2);

        assert_eq!(codec.decode(&felts), Some(format!("account-id({hex})")));
    }

    #[test]
    fn invalid_account_id_token_is_rejected() {
        let err = AccountIdCodec.encode("not-hex").unwrap_err();
        assert!(matches!(err, TypedDebugInfoError::InvalidScalar { .. }));
    }
}

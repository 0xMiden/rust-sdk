//! The `account-id` codec for typed `call` rendering.
//!
//! `account-id` felts are validated with protocol-level rules, so the CLI registers this codec
//! (via [`TypedProcInfo::with_scalar_codec`]) to encode one account ID token, hex or bech32, into
//! the two stack felts the procedure expects and render the returned felts back as
//! `account-id(0x..)`.
//!
//! [`TypedProcInfo::with_scalar_codec`]: miden_client::vm::typed::TypedProcInfo::with_scalar_codec

use miden_client::Felt;
use miden_client::account::AccountId;
use miden_client::vm::typed::{MIDEN_CORE_TYPES, TypedError, WitScalarCodec};

use crate::codecs::{ACCOUNT_ID_WIT_NAME, parse_account_id_token};

/// Encodes and renders the WIT `account-id` type: one token, hex or bech32, two stack felts.
pub struct AccountIdCodec;

impl WitScalarCodec for AccountIdCodec {
    fn wit_name(&self) -> &str {
        ACCOUNT_ID_WIT_NAME
    }

    fn wit_interface(&self) -> Option<&str> {
        Some(MIDEN_CORE_TYPES)
    }

    fn encode(&self, token: &str) -> Result<Vec<Felt>, TypedError> {
        let id = parse_account_id_token(token)?;
        Ok(vec![id.prefix().into(), id.suffix()])
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
    use miden_client::address::{Address, NetworkId};

    use super::*;

    /// A valid account ID, used in both spellings.
    const HEX_ID: &str = "0xaa0000000000bb110000cc000000dd";

    #[test]
    fn account_id_one_hex_token_roundtrips() {
        let codec = AccountIdCodec;
        let hex = HEX_ID;
        let id = AccountId::from_hex(hex).unwrap();

        // Compared against the felts the account id itself carries: a round-trip alone would also
        // pass if `encode` and `decode` had the two fields the same way around.
        let expected = [Felt::from(id.prefix()), id.suffix()];
        assert_eq!(codec.encode(hex).unwrap(), expected);

        assert_eq!(codec.decode(&expected).unwrap(), format!("account-id({hex})"));
    }

    #[test]
    fn felts_that_are_not_an_account_id_are_rejected() {
        let err = AccountIdCodec.decode(&[Felt::from(1u32), Felt::from(2u32)]).unwrap_err();
        assert!(matches!(err, TypedError::MalformedResult { .. }));
    }

    #[test]
    fn a_bech32_token_encodes_to_the_same_felts_as_its_hex_spelling() {
        // `call` resolves its target through `parse_account_id`, which takes bech32, so an
        // argument of the same type has to reach the same account from either spelling.
        let id = AccountId::from_hex(HEX_ID).unwrap();
        let bech32 = Address::new(id).encode(NetworkId::Testnet);

        assert_eq!(
            AccountIdCodec.encode(&bech32).unwrap(),
            AccountIdCodec.encode(&id.to_hex()).unwrap()
        );
    }

    #[test]
    fn an_invalid_account_id_token_is_rejected() {
        // A `0x` token is read as hex and anything else as a bech32 address, so a token that is
        // neither has to be rejected under both readings.
        let valid_bech32 =
            Address::new(AccountId::from_hex(HEX_ID).unwrap()).encode(NetworkId::Testnet);
        let tokens = [
            "not-hex",
            "0xnothex",
            // Valid bech32 up to the last character, which breaks its checksum.
            &valid_bech32[..valid_bech32.len() - 1],
        ];

        for token in tokens {
            let err = AccountIdCodec.encode(token).unwrap_err();
            assert!(matches!(err, TypedError::InvalidScalar { .. }), "token '{token}' accepted");
        }
    }
}

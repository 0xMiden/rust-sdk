//! The `asset` codec for typed `call` rendering.
//!
//! On the stack a WIT `asset` is its eight felts — the id word followed by the value word, i.e.
//! [`Asset::as_elements`]. The CLI registers this codec so an asset argument can be given as a
//! single `<AMOUNT>::<FAUCET_ID>` token instead of two raw word hexes, and so a returned asset
//! renders back the same way. The token form matches the one the rest of the CLI takes for
//! fungible assets, minus the token symbol and address spellings: resolving those needs the
//! client, and a codec only sees the text.

use miden_client::account::AccountId;
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::vm::typed::{MIDEN_CORE_TYPES, TypedError, WitScalarCodec};
use miden_client::{Felt, Word};

use crate::codecs::invalid_scalar;

/// Bare WIT type name the typed encoder matches this codec against (e.g. the leaf of
/// `miden:base/core-types@1.0.0/asset`).
const ASSET_WIT_NAME: &str = "asset";

/// Encodes and renders the WIT `asset` type: one `<AMOUNT>::<FAUCET_ID>` token, eight stack felts.
/// Only fungible assets are supported by this token form.
pub struct AssetCodec;

impl WitScalarCodec for AssetCodec {
    fn wit_name(&self) -> &str {
        ASSET_WIT_NAME
    }

    fn wit_interface(&self) -> Option<&str> {
        Some(MIDEN_CORE_TYPES)
    }

    fn encode(&self, token: &str) -> Result<Vec<Felt>, TypedError> {
        let (amount, faucet) = token.split_once("::").ok_or_else(|| {
            invalid_scalar(ASSET_WIT_NAME, token, "expected `<AMOUNT>::<FAUCET_ID>`")
        })?;
        let amount: u64 = amount.parse().map_err(|e: core::num::ParseIntError| {
            invalid_scalar(ASSET_WIT_NAME, token, &format!("invalid amount: {e}"))
        })?;
        let faucet_id =
            AccountId::from_hex(faucet).map_err(|e| invalid_scalar(ASSET_WIT_NAME, token, &e))?;
        let asset: Asset = FungibleAsset::new(faucet_id, amount)
            .map_err(|e| invalid_scalar(ASSET_WIT_NAME, token, &e))?
            .into();
        Ok(asset.as_elements().to_vec())
    }

    fn decode(&self, felts: &[Felt]) -> Result<String, TypedError> {
        // As in `AccountIdCodec`, any count other than the type's width means the signature and
        // this codec disagree.
        let [k0, k1, k2, k3, v0, v1, v2, v3] = felts else {
            return Err(malformed_asset("an asset occupies exactly eight felts"));
        };
        let id = Word::from([*k0, *k1, *k2, *k3]);
        let value = Word::from([*v0, *v1, *v2, *v3]);
        let asset = Asset::from_id_and_value_words(id, value)
            .map_err(|_| malformed_asset("the felts are not a valid asset"))?;
        Ok(match asset {
            Asset::Fungible(f) => format!("asset({}::{})", f.amount(), f.faucet_id().to_hex()),
            Asset::NonFungible(_) => "asset(non-fungible)".to_string(),
        })
    }
}

/// Builds the error for result felts that are not an asset this codec can render.
fn malformed_asset(reason: &'static str) -> TypedError {
    TypedError::MalformedResult { ty: ASSET_WIT_NAME.to_string(), reason }
}

#[cfg(test)]
mod tests {
    use miden_client::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;

    use super::*;

    fn hex(id: u128) -> String {
        AccountId::try_from(id).unwrap().to_hex()
    }

    fn faucet_token(amount: u64) -> String {
        format!("{amount}::{}", hex(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET))
    }

    #[test]
    fn a_fungible_asset_token_roundtrips() {
        let token = faucet_token(100);

        let felts = AssetCodec.encode(&token).unwrap();
        assert_eq!(felts.len(), 8);

        assert_eq!(AssetCodec.decode(&felts).unwrap(), format!("asset({token})"));
    }

    #[test]
    fn a_token_with_a_single_colon_is_rejected() {
        let err = AssetCodec.encode(&faucet_token(100).replace("::", ":")).unwrap_err();
        assert!(matches!(err, TypedError::InvalidScalar { .. }));
    }

    #[test]
    fn a_token_in_the_reverse_order_is_rejected() {
        let token = format!("{}::100", hex(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET));
        let err = AssetCodec.encode(&token).unwrap_err();
        assert!(matches!(err, TypedError::InvalidScalar { .. }));
    }

    #[test]
    fn felts_that_are_not_an_asset_are_rejected() {
        let felts: Vec<Felt> = (1..=8u32).map(Felt::from).collect();
        let err = AssetCodec.decode(&felts).unwrap_err();
        assert!(matches!(err, TypedError::MalformedResult { .. }));
    }

    #[test]
    fn a_felt_count_other_than_eight_is_rejected() {
        let felts = AssetCodec.encode(&faucet_token(100)).unwrap();

        for len in [0, 4, 7] {
            let err = AssetCodec.decode(&felts[..len]).unwrap_err();
            assert!(matches!(err, TypedError::MalformedResult { .. }), "len {len} was accepted");
        }
    }
}

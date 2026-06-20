//! The `asset` codec for typed `call` rendering.
//!
//! On the stack a WIT `asset` is its eight felts — the key word followed by the value word, i.e.
//! [`Asset::as_elements`]. The CLI registers this codec so an asset argument can be given as a
//! single `<faucet_id_hex>:<amount>` token instead of two raw word hexes, and so a returned asset
//! renders back the same way.

use miden_mast_package::debug_info::typed::{TypedDebugInfoError, WitScalarCodec};
use miden_protocol::account::AccountId;
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::{Felt, Word};

use crate::codecs::invalid_scalar;

/// Bare WIT type name the typed encoder matches this codec against (e.g. the leaf of
/// `miden:base/core-types@1.0.0/asset`).
const ASSET_WIT_NAME: &str = "asset";

/// Encodes and renders the WIT `asset` type: one `<faucet_id_hex>:<amount>` token, eight stack
/// felts. Only fungible assets are supported by this token form.
pub struct AssetCodec;

impl WitScalarCodec for AssetCodec {
    fn wit_name(&self) -> &str {
        ASSET_WIT_NAME
    }

    fn felt_count(&self) -> usize {
        8
    }

    fn encode(&self, token: &str) -> Result<Vec<Felt>, TypedDebugInfoError> {
        let (faucet, amount) = token.split_once(':').ok_or_else(|| {
            invalid_scalar(ASSET_WIT_NAME, token, "expected `<faucet_id_hex>:<amount>`")
        })?;
        let faucet_id =
            AccountId::from_hex(faucet).map_err(|e| invalid_scalar(ASSET_WIT_NAME, token, &e))?;
        let amount: u64 = amount.parse().map_err(|e: core::num::ParseIntError| {
            invalid_scalar(ASSET_WIT_NAME, token, &format!("invalid amount: {e}"))
        })?;
        let asset: Asset = FungibleAsset::new(faucet_id, amount)
            .map_err(|e| invalid_scalar(ASSET_WIT_NAME, token, &e))?
            .into();
        Ok(asset.as_elements().to_vec())
    }

    fn decode(&self, felts: &[Felt]) -> Option<String> {
        if felts.len() < 8 {
            return None;
        }
        let key = Word::from([felts[0], felts[1], felts[2], felts[3]]);
        let value = Word::from([felts[4], felts[5], felts[6], felts[7]]);
        match Asset::from_key_value_words(key, value).ok()? {
            Asset::Fungible(f) => Some(format!("asset({}:{})", f.faucet_id().to_hex(), f.amount())),
            Asset::NonFungible(_) => Some("asset(non-fungible)".to_string()),
        }
    }
}

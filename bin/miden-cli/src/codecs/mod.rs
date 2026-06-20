//! CLI-side WIT scalar codecs for typed `call` rendering.
//!
//! The generic typed encode/decode engine and the [`WitScalarCodec`] trait live in
//! `miden-mast-package`. The protocol-aware codecs — which need `miden-protocol` types such as
//! `AccountId` and `Asset` to parse and render their friendly token form — live here, and are
//! registered onto a [`TypedProcInfo`] in one place via [`with_cli_codecs`]. Commands call that
//! helper instead of knowing about individual types.
//!
//! [`WitScalarCodec`]: miden_mast_package::debug_info::typed::WitScalarCodec
//! [`TypedProcInfo`]: miden_mast_package::debug_info::typed::TypedProcInfo

mod account_id;
mod asset;

pub use account_id::AccountIdCodec;
pub use asset::AssetCodec;
use miden_mast_package::debug_info::typed::{TypedDebugInfoError, TypedProcInfo};

/// Builds the `InvalidScalar` error a codec returns when it can't parse `token`. Shared so every
/// codec reports the same error shape from one place.
pub(crate) fn invalid_scalar(
    wit_name: &str,
    token: &str,
    reason: &(impl ToString + ?Sized),
) -> TypedDebugInfoError {
    TypedDebugInfoError::InvalidScalar {
        wit_name: wit_name.to_string(),
        token: token.to_string(),
        reason: reason.to_string(),
    }
}

/// Registers every CLI scalar codec onto `typed`. New codecs are added here so the commands that
/// render typed signatures stay agnostic of the individual WIT types.
pub fn with_cli_codecs(typed: TypedProcInfo) -> TypedProcInfo {
    typed
        .with_scalar_codec(Box::new(AccountIdCodec))
        .with_scalar_codec(Box::new(AssetCodec))
}

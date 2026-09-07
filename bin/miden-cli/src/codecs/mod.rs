//! Protocol-aware WIT scalar codecs.
//!
//! The encode/decode engine and the [`WitScalarCodec`] trait live in `miden-assembly-syntax`, which
//! does not depend on `miden-protocol` and should not: the VM does not depend on the protocol. So
//! it ships the two codecs it can write itself, `word` and `felt`, and leaves the trait for the
//! rest.
//!
//! `account-id` and `asset` are the rest. `AccountId::from_hex` says what a valid id is, and
//! `Asset` says what a valid asset is, so both codecs live on this side.
//!
//! [`with_cli_codecs`] registers them in one place, so the commands that render typed signatures
//! do not know the individual types.
//!
//! [`WitScalarCodec`]: miden_client::vm::typed::WitScalarCodec
//! [`TypedProcInfo`]: miden_client::vm::typed::TypedProcInfo

use miden_client::vm::typed::{TypedError, TypedProcInfo};

mod account_id;
mod asset;

pub use account_id::AccountIdCodec;
pub use asset::AssetCodec;

/// Builds the `InvalidScalar` error a codec returns when it can't parse `token`. Shared so every
/// codec reports the same error shape from one place.
pub(crate) fn invalid_scalar(
    wit_name: &str,
    token: &str,
    reason: &(impl ToString + ?Sized),
) -> TypedError {
    TypedError::InvalidScalar {
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

//! Protocol-aware WIT scalar codecs.
//!
//! The encode/decode engine and the [`WitScalarCodec`] trait live in `miden-assembly-syntax`, which
//! does not depend on `miden-protocol` and should not: the VM does not depend on the protocol. So
//! it ships the two codecs it can write itself, `word` and `felt`, and leaves the trait for the
//! rest.
//!
//! `account-id` and `asset` are the rest. `AccountId` says what a valid id is, and `Asset` says
//! what a valid asset is, so both codecs live on this side.
//!
//! [`with_cli_codecs`] registers them in one place, so the commands that render typed signatures do
//! not know the individual types.
//!
//! [`WitScalarCodec`]: miden_client::vm::typed::WitScalarCodec
//! [`TypedProcInfo`]: miden_client::vm::typed::TypedProcInfo

use miden_client::account::AccountId;
use miden_client::address::{Address, AddressId};
use miden_client::vm::typed::{TypedError, TypedProcInfo};

mod account_id;
mod asset;

pub use account_id::AccountIdCodec;
pub use asset::AssetCodec;

/// Bare WIT type name of the core `account-id` type, regardless of the package and version in the
/// full type name (e.g. `miden:base/core-types@1.0.0/account-id`). It is both what the typed
/// encoder matches [`AccountIdCodec`] against and the label every account ID token is read under,
/// including the faucet half of an `asset`.
pub(crate) const ACCOUNT_ID_WIT_NAME: &str = "account-id";

/// Reads an account ID written either way the rest of the CLI takes one: as full hex, or as a
/// bech32 address naming an account ID. Both spellings reach the CLI in one command line, since
/// `call` resolves its target through [`parse_account_id`], so an argument that takes an account ID
/// has to accept the same two.
///
/// [`parse_account_id`]: crate::utils::parse_account_id
pub(crate) fn parse_account_id_token(token: &str) -> Result<AccountId, TypedError> {
    // The prefix picks the spelling, so a mistyped hex ID is reported as bad hex rather than as a
    // bad bech32 address.
    if token.starts_with("0x") {
        return AccountId::from_hex(token)
            .map_err(|err| invalid_scalar(ACCOUNT_ID_WIT_NAME, token, &err));
    }

    let (_, address) =
        Address::decode(token).map_err(|err| invalid_scalar(ACCOUNT_ID_WIT_NAME, token, &err))?;
    match address.id() {
        AddressId::AccountId(id) => Ok(id),
        _ => Err(invalid_scalar(
            ACCOUNT_ID_WIT_NAME,
            token,
            "the address doesn't name an account ID",
        )),
    }
}

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

use alloc::string::String;

use thiserror::Error;

/// Error returned while talking to a `Web3Signer` instance.
#[derive(Debug, Error)]
pub enum Web3SignerError {
    /// The request could not be completed, or the signer answered with a non-success status.
    #[error("request to `{path}` failed: {message}")]
    Transport { path: String, message: String },

    /// The signer holds no keys. Reported when building the authenticator, because an empty
    /// key list means every later signing request would fail.
    #[error("the signer reported no keys")]
    NoKeys,

    /// A response field that should be hex is not.
    #[error("`{value}` is not hex: {message}")]
    InvalidHex { value: String, message: String },

    /// A listed key is not a secp256k1 public key.
    #[error("`{identifier}` is not a secp256k1 public key: {message}")]
    InvalidPublicKey { identifier: String, message: String },

    /// The returned signature is not exactly 65 bytes long.
    #[error("signature for `{identifier}` is {got} bytes, expected 65")]
    InvalidSignatureLength { identifier: String, got: usize },

    /// The returned `v` is neither a recovery id nor a recovery id offset by 27.
    #[error("signature for `{identifier}` carries `v = {v}`, which is not a recovery id")]
    InvalidRecoveryId { identifier: String, v: u8 },
}

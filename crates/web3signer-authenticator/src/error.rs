use alloc::string::String;

use thiserror::Error;

use crate::decode::SIGNATURE_LEN;

/// Error returned while talking to a `Web3Signer` instance.
#[derive(Debug, Error)]
pub enum Web3SignerError {
    /// The request could not be completed, or the signer answered with a non-success status.
    #[error("request to `{url}` failed: {message}")]
    Transport { url: String, message: String },
    /// The web3 signer holds no keys.
    #[error("the signer reported no keys")]
    NoKeys,
    /// A response field that should be hex is not.
    #[error("`{value}` is not hex: {message}")]
    InvalidHex { value: String, message: String },
    /// A listed key is not a secp256k1 public key.
    #[error("`{identifier}` is not a secp256k1 public key: {message}")]
    InvalidPublicKey { identifier: String, message: String },
    /// The returned signature is not exactly 65 bytes long.
    #[error("the signature is {length} bytes, expected {SIGNATURE_LEN}")]
    InvalidSignatureLength { length: usize },
    /// The returned `v` component of a signature is neither a recovery id nor a recovery id offset
    /// by 27.
    #[error("the signature carries `v = {v}`, which is not a valid recovery id")]
    InvalidRecoveryId { v: u8 },
}

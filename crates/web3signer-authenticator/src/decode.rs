use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::auth::Signature;
use miden_protocol::crypto::dsa::ecdsa_k256_keccak;

use crate::Web3SignerError;

/// Length of a `Web3Signer` secp256k1 signature: `r || s || v`.
pub(crate) const SIGNATURE_BYTES: usize = 65;

/// Length of the `r || s` part of a signature.
pub(crate) const SCALARS_BYTES: usize = 64;

/// Splits a flat JSON array of strings, `["0x..","0x.."]`, into its entries, stripping quotes and
/// whitespace and skipping empty ones.
///
/// The `publicKeys` response is exactly this shape, so parsing it needs no JSON dependency in the
/// `no_std` core.
pub(crate) fn parse_string_array(body: &str) -> impl Iterator<Item = &str> {
    body.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|entry| entry.trim().trim_matches('"').trim())
        .filter(|entry| !entry.is_empty())
}

/// Decodes a hex string, tolerating a `0x` prefix, surrounding whitespace and the quotes of a JSON
/// string body.
pub(crate) fn decode_hex(value: &str, identifier: &str) -> Result<Vec<u8>, Web3SignerError> {
    let value = value.trim().trim_matches('"').trim();

    hex::decode(value.strip_prefix("0x").unwrap_or(value)).map_err(|err| {
        Web3SignerError::InvalidHex {
            identifier: identifier.to_string(),
            message: err.to_string(),
        }
    })
}

/// Decodes an `r || s || v` signature as returned by `Web3Signer` and checks that it belongs to
/// `verifying_key`.
pub(crate) fn decode_signature(
    response: &str,
    identifier: &str,
    message: Word,
    verifying_key: &ecdsa_k256_keccak::PublicKey,
) -> Result<Signature, Web3SignerError> {
    let bytes = decode_hex(response, identifier)?;
    let bytes: [u8; SIGNATURE_BYTES] =
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| Web3SignerError::InvalidSignatureLength {
                identifier: identifier.to_string(),
                got: bytes.len(),
            })?;

    let scalars: [u8; SCALARS_BYTES] =
        bytes[..SCALARS_BYTES].try_into().expect("slice is exactly the scalars");

    // `Web3Signer` reports `v` the way web3j writes it, as the recovery id offset by 27. A plain
    // recovery id is taken as it is, and anything else is rejected below.
    let recovery_id = match bytes[SCALARS_BYTES] {
        v @ 27..=30 => v - 27,
        v => v,
    };

    let signature =
        ecdsa_k256_keccak::Signature::from_sec1_bytes_and_recovery_id(scalars, recovery_id)
            .map_err(|_| Web3SignerError::InvalidRecoveryId {
                identifier: identifier.to_string(),
                v: bytes[SCALARS_BYTES],
            })?;

    // Recovering the key both checks the signature against the key it was requested for and
    // guarantees that `Signature::to_encoded_signature`, which recovers it again while encoding
    // the signature for the VM, will not fail later on.
    let recovered = ecdsa_k256_keccak::PublicKey::recover_from(message, &signature).ok();
    if recovered.as_ref() != Some(verifying_key) {
        return Err(Web3SignerError::SignatureDoesNotVerify { identifier: identifier.to_string() });
    }

    Ok(Signature::EcdsaK256Keccak(signature))
}

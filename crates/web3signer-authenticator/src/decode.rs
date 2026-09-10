use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use k256::ecdsa::VerifyingKey;
use miden_protocol::account::auth::Signature;
use miden_protocol::crypto::dsa::ecdsa_k256_keccak;
use miden_protocol::utils::serde::Deserializable;

use crate::Web3SignerError;

// CONSTANTS
// ================================================================================================

/// Length of the `r || s` scalars of a secp256k1 signature.
const SCALARS_LEN: usize = 64;

/// Length of a `Web3Signer` secp256k1 signature: `r || s || v`.
pub(crate) const SIGNATURE_LEN: usize = SCALARS_LEN + 1;

/// Length of a public key in its compressed form.
const COMPRESSED_KEY_LEN: usize = 33;

/// Length of a public key in its full form without the leading tag byte, which is how the signer
/// reports it.
const UNTAGGED_KEY_LEN: usize = 64;

/// Length of a public key in its full form, tag byte included.
const UNCOMPRESSED_KEY_LEN: usize = 65;

/// Tag byte that marks a key as being in its full form.
const UNCOMPRESSED_TAG: u8 = 0x04;

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

/// Decodes a public key the signer reported.
///
/// A secp256k1 public key has three encodings: compressed, full, and full without its leading tag
/// byte. The signer uses the last one, and `ecdsa_k256_keccak` reads only the compressed one, so
/// every key is parsed here and re-encoded compressed.
pub(crate) fn decode_public_key(
    public_key_hex: &str,
) -> Result<ecdsa_k256_keccak::PublicKey, Web3SignerError> {
    let bytes = decode_hex(public_key_hex)?;

    let sec1 = match bytes.len() {
        UNTAGGED_KEY_LEN => [&[UNCOMPRESSED_TAG][..], &bytes].concat(),
        COMPRESSED_KEY_LEN | UNCOMPRESSED_KEY_LEN => bytes,
        other => {
            return Err(Web3SignerError::InvalidPublicKey {
                identifier: public_key_hex.to_string(),
                message: format!(
                    "{other} bytes, expected {COMPRESSED_KEY_LEN} (compressed), \
                     {UNTAGGED_KEY_LEN} (full, as the signer reports it) or \
                     {UNCOMPRESSED_KEY_LEN} (full with its tag byte)"
                ),
            });
        },
    };

    let invalid_key = |message: String| Web3SignerError::InvalidPublicKey {
        identifier: public_key_hex.to_string(),
        message,
    };

    let verifying_key =
        VerifyingKey::from_sec1_bytes(&sec1).map_err(|err| invalid_key(err.to_string()))?;

    ecdsa_k256_keccak::PublicKey::read_from_bytes(verifying_key.to_sec1_point(true).as_bytes())
        .map_err(|err| invalid_key(err.to_string()))
}

/// Decodes a hex string, tolerating a `0x` prefix, surrounding whitespace and the quotes of a JSON
/// string body.
pub(crate) fn decode_hex(value: &str) -> Result<Vec<u8>, Web3SignerError> {
    let value = value.trim().trim_matches('"').trim();

    hex::decode(value.strip_prefix("0x").unwrap_or(value)).map_err(|err| {
        Web3SignerError::InvalidHex {
            value: value.to_string(),
            message: err.to_string(),
        }
    })
}

// WEB3 SIGNATURE
// ================================================================================================

/// A signature as `Web3Signer` returns it: the `r || s` scalars and the recovery id its `v`
/// component carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Web3Signature {
    scalars: [u8; SCALARS_LEN],
    recovery_id: u8,
}

impl Web3Signature {
    /// Decodes an `r || s || v` signature as returned by `Web3Signer`.
    ///
    /// `v` is a recovery id, which the signer may report offset by 27, so both forms decode to the
    /// same recovery id.
    ///
    /// # Errors
    /// Returns an error if the signature is not hex or is not [`SIGNATURE_LEN`] bytes long.
    pub(crate) fn from_hex(signature_hex: &str) -> Result<Self, Web3SignerError> {
        let decoded = decode_hex(signature_hex)?;
        let signature_bytes: [u8; SIGNATURE_LEN] = decoded
            .as_slice()
            .try_into()
            .map_err(|_| Web3SignerError::InvalidSignatureLength { length: decoded.len() })?;

        let mut scalars = [0u8; SCALARS_LEN];
        scalars.copy_from_slice(&signature_bytes[..SCALARS_LEN]);

        let recovery_id = match signature_bytes[SCALARS_LEN] {
            offset @ 27..=30 => offset - 27,
            recovery_id => recovery_id,
        };

        Ok(Self { scalars, recovery_id })
    }

    /// Converts the signature into the protocol's representation.
    ///
    /// # Errors
    /// Returns an error if the recovery id is not one a secp256k1 signature can carry.
    pub(crate) fn to_signature(self) -> Result<Signature, Web3SignerError> {
        let signature = ecdsa_k256_keccak::Signature::from_sec1_bytes_and_recovery_id(
            self.scalars,
            self.recovery_id,
        )
        .map_err(|_| Web3SignerError::InvalidRecoveryId { v: self.recovery_id })?;

        Ok(Signature::EcdsaK256Keccak(signature))
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::format;

    use super::*;

    /// A key as `Web3Signer` reports it: full form, no leading tag byte.
    const UNCOMPRESSED: &str = "0x09b02f8a5fddd222ade4ea4528faefc399623af3f736be3c44f03e2df22fb792\
                                f3931a4d9573d333ca74343305762a753388c3422a86d98b713fc91c1ea04842";

    /// The same key, compressed.
    const COMPRESSED: &str = "0x0209b02f8a5fddd222ade4ea4528faefc399623af3f736be3c44f03e2df22fb792";

    #[test]
    fn a_v_offset_by_27_decodes_to_the_same_recovery_id() {
        let scalars = "11".repeat(SCALARS_LEN);

        let offset = Web3Signature::from_hex(&format!("0x{scalars}1c")).expect("v = 28 decodes");
        let plain = Web3Signature::from_hex(&format!("0x{scalars}01")).expect("v = 1 decodes");

        assert_eq!(offset, plain);
        assert_eq!(offset.recovery_id, 1);
    }

    #[test]
    fn every_key_encoding_decodes_to_the_same_key() {
        let tagged = format!("0x04{}", UNCOMPRESSED.trim_start_matches("0x"));

        let compressed = decode_public_key(COMPRESSED).expect("compressed key decodes");
        let uncompressed = decode_public_key(UNCOMPRESSED).expect("uncompressed key decodes");
        let tagged = decode_public_key(&tagged).expect("tagged key decodes");

        assert_eq!(compressed, uncompressed);
        assert_eq!(compressed, tagged);
    }
}

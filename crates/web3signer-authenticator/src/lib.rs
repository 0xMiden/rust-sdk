//! A [`TransactionAuthenticator`] that signs with keys held in a `Web3Signer` instance.
//!
//! The signer's key list is read once, when the authenticator is built, and the authenticator
//! signs only for the keys it found there. It holds no key material and has no fallback signer
//! under any configuration: a public key commitment that is not in the key list is an error rather
//! than a request that gets served locally, so a vault that is misconfigured or points at the
//! wrong instance cannot be mistaken for a working one.
//!
//! Only the `EcdsaK256Keccak` authentication scheme is supported, since that is the only scheme
//! `Web3Signer` can produce. The authenticator neither creates, imports nor exports keys.
//!
//! ```no_run
//! use miden_client_web3signer::{Web3SignerAuthenticator, Web3SignerConfig, Web3SignerError};
//!
//! # async fn example() -> Result<(), Web3SignerError> {
//! let authenticator =
//!     Web3SignerAuthenticator::connect("http://127.0.0.1:9000", Web3SignerConfig::default())
//!         .await?;
//! # Ok(())
//! # }
//! ```

#![no_std]

extern crate alloc;

#[cfg(any(feature = "std", test))]
extern crate std;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;

use miden_protocol::Word;
use miden_protocol::account::auth::{PublicKey, PublicKeyCommitment, Signature};
use miden_protocol::crypto::dsa::ecdsa_k256_keccak;
use miden_protocol::vm::FutureMaybeSend;
use miden_tx::AuthenticationError;
use miden_tx::auth::{SigningInputs, TransactionAuthenticator};

use crate::decode::{decode_public_key, decode_signature, parse_string_array};

mod decode;
mod error;
mod transport;

pub use error::Web3SignerError;
pub use transport::SignerTransport;

#[cfg(feature = "std")]
mod http;
#[cfg(feature = "std")]
pub use http::{HttpTransport, Web3SignerConfig};

// CONSTANTS
// ================================================================================================

/// Endpoint listing the keys the signer holds.
const PUBLIC_KEYS_PATH: &str = "/api/v1/eth1/publicKeys";

/// Endpoint prefix for a signing request; the key's identifier completes it.
const SIGN_PATH_PREFIX: &str = "/api/v1/eth1/sign/";

// WEB3SIGNER AUTHENTICATOR
// ================================================================================================

/// One key of the signer's key list.
struct KeyEntry {
    /// Handed out by [`TransactionAuthenticator::get_public_key`].
    public_key: Arc<PublicKey>,
    /// The same key, kept unwrapped for the recovery check in [`decode_signature`].
    verifying_key: ecdsa_k256_keccak::PublicKey,
    /// The key's identifier as the signer reported it, used verbatim in the signing URL.
    identifier: String,
}

/// A [`TransactionAuthenticator`] backed by a `Web3Signer` instance.
///
/// See the [crate-level documentation](crate) for the guarantees this makes.
pub struct Web3SignerAuthenticator<T> {
    transport: T,
    keys: BTreeMap<PublicKeyCommitment, KeyEntry>,
}

#[cfg(feature = "std")]
impl Web3SignerAuthenticator<HttpTransport> {
    /// Reads the key list of the `Web3Signer` instance at `url` over HTTP and builds the
    /// authenticator from it.
    ///
    /// # Errors
    /// Returns an error if the instance cannot be reached, holds no keys, or lists a key that is
    /// not a secp256k1 public key.
    pub async fn connect(url: &str, config: Web3SignerConfig) -> Result<Self, Web3SignerError> {
        Self::connect_with(HttpTransport::new(url, config)?).await
    }
}

impl<T: SignerTransport> Web3SignerAuthenticator<T> {
    /// Reads the key list over the given transport and builds the authenticator from it.
    pub async fn connect_with(transport: T) -> Result<Self, Web3SignerError> {
        let body = transport.get(PUBLIC_KEYS_PATH).await?;

        let mut keys = BTreeMap::new();
        for identifier in parse_string_array(&body) {
            let verifying_key = decode_public_key(identifier)?;

            let public_key = PublicKey::EcdsaK256Keccak(verifying_key.clone());
            keys.insert(
                public_key.to_commitment(),
                KeyEntry {
                    public_key: Arc::new(public_key),
                    verifying_key,
                    identifier: identifier.to_string(),
                },
            );
        }

        if keys.is_empty() {
            return Err(Web3SignerError::NoKeys);
        }

        Ok(Self { transport, keys })
    }

    /// Returns the public key commitments this authenticator can sign for.
    pub fn public_key_commitments(&self) -> impl Iterator<Item = PublicKeyCommitment> + '_ {
        self.keys.keys().copied()
    }

    /// Requests a signature over `message` for one key of the key list.
    async fn request_signature(
        &self,
        entry: &KeyEntry,
        message: Word,
    ) -> Result<Signature, Web3SignerError> {
        // `Web3Signer` hashes the payload with keccak256 before signing, which is exactly what
        // `ecdsa_k256_keccak` signing does to the 32 bytes of a message word, so the payload is
        // the message word itself and nothing is hashed here.
        let data = hex::encode(<[u8; 32]>::from(message));
        let path = format!("{SIGN_PATH_PREFIX}{}", entry.identifier);

        let response = self.transport.post(&path, format!("{{\"data\":\"0x{data}\"}}")).await?;

        decode_signature(&response, &entry.identifier, message, &entry.verifying_key)
    }
}

impl<T: SignerTransport> TransactionAuthenticator for Web3SignerAuthenticator<T> {
    /// Requests a signature over the signing inputs from the `Web3Signer` instance.
    ///
    /// # Errors
    /// Returns [`AuthenticationError::UnknownPublicKey`] if the commitment is not one the signer
    /// reported when the authenticator was built. No signature is requested in that case, and
    /// none is produced locally.
    fn get_signature(
        &self,
        pub_key_commitment: PublicKeyCommitment,
        signing_inputs: &SigningInputs,
    ) -> impl FutureMaybeSend<Result<Signature, AuthenticationError>> {
        let message = signing_inputs.to_commitment();

        async move {
            let entry = self
                .keys
                .get(&pub_key_commitment)
                .ok_or(AuthenticationError::UnknownPublicKey(pub_key_commitment))?;

            self.request_signature(entry, message).await.map_err(|err| {
                AuthenticationError::other_with_source("web3signer failed to sign", err)
            })
        }
    }

    /// Retrieves a public key for a specific public key commitment.
    fn get_public_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> impl FutureMaybeSend<Option<Arc<PublicKey>>> {
        let public_key = self.keys.get(&pub_key_commitment).map(|entry| entry.public_key.clone());

        async move { public_key }
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::dsa::ecdsa_k256_keccak::SigningKey;
    use miden_protocol::utils::serde::{Deserializable, Serializable};

    use super::*;
    use crate::decode::{SCALARS_BYTES, SIGNATURE_BYTES};

    /// A transport that answers the way a `Web3Signer` instance holding a single key does.
    struct MockTransport {
        signing_key: SigningKey,
        /// Set when a signing request is received, to assert that unknown keys don't reach the
        /// signer.
        signed: core::sync::atomic::AtomicBool,
        /// Number of signature bytes to answer with, to exercise the length check.
        signature_bytes: usize,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                // Any 32 bytes below the curve order are a valid key; a fixed one keeps the
                // test deterministic without pulling in an RNG.
                signing_key: SigningKey::read_from_bytes(&[7; 32]).expect("key is in range"),
                signed: core::sync::atomic::AtomicBool::new(false),
                signature_bytes: SIGNATURE_BYTES,
            }
        }

        fn identifier(&self) -> String {
            format!("0x{}", hex::encode(self.signing_key.public_key().to_bytes()))
        }
    }

    impl SignerTransport for MockTransport {
        #[allow(clippy::unused_async_trait_impl)]
        async fn get(&self, path: &str) -> Result<String, Web3SignerError> {
            assert_eq!(path, PUBLIC_KEYS_PATH);

            Ok(format!("[\n  \"{}\"\n]", self.identifier()))
        }

        #[allow(clippy::unused_async_trait_impl)]
        async fn post(&self, path: &str, body: String) -> Result<String, Web3SignerError> {
            self.signed.store(true, core::sync::atomic::Ordering::Relaxed);
            assert_eq!(path, format!("{SIGN_PATH_PREFIX}{}", self.identifier()));

            // The signer hashes the payload itself, so signing the word the payload decodes to
            // reproduces what a real instance returns only if the payload is the message word.
            let data = body
                .strip_prefix("{\"data\":\"0x")
                .and_then(|body| body.strip_suffix("\"}"))
                .expect("body is a data object");
            let data: [u8; 32] = hex::decode(data).unwrap().try_into().unwrap();
            let signature = self.signing_key.sign(Word::try_from(data).unwrap());

            let mut bytes = signature.to_sec1_bytes().to_vec();
            bytes.push(signature.v() + 27);

            Ok(format!("0x{}", hex::encode(&bytes[..self.signature_bytes])))
        }
    }

    fn signing_inputs() -> SigningInputs {
        SigningInputs::Blind(Word::from([9u32, 8, 7, 6]))
    }

    #[tokio::test]
    async fn signs_with_a_key_of_the_key_list() {
        let authenticator = Web3SignerAuthenticator::connect_with(MockTransport::new())
            .await
            .expect("key list is readable");

        let commitment = authenticator
            .public_key_commitments()
            .next()
            .expect("key list holds the mock key");
        let public_key = authenticator
            .get_public_key(commitment)
            .await
            .expect("commitment is in the key list");

        let inputs = signing_inputs();
        let signature = authenticator
            .get_signature(commitment, &inputs)
            .await
            .expect("mock signer signs");

        let message = inputs.to_commitment();
        assert!(public_key.verify(message, signature.clone()));
        // Panics if the signature does not encode for the VM.
        signature.to_encoded_signature(message);
    }

    #[tokio::test]
    async fn unknown_commitment_is_rejected_without_signing() {
        let authenticator = Web3SignerAuthenticator::connect_with(MockTransport::new())
            .await
            .expect("key list is readable");

        let unknown = PublicKeyCommitment::from(Word::from([1u32, 2, 3, 4]));
        let error = authenticator
            .get_signature(unknown, &signing_inputs())
            .await
            .expect_err("commitment is not in the key list");

        assert!(matches!(error, AuthenticationError::UnknownPublicKey(_)));
        assert!(
            !authenticator.transport.signed.load(core::sync::atomic::Ordering::Relaxed),
            "an unknown commitment must not reach the signer"
        );
        assert!(authenticator.get_public_key(unknown).await.is_none());
    }

    #[tokio::test]
    async fn signature_of_the_wrong_length_is_rejected() {
        let mut transport = MockTransport::new();
        transport.signature_bytes = SCALARS_BYTES;

        let authenticator = Web3SignerAuthenticator::connect_with(transport)
            .await
            .expect("key list is readable");
        let commitment = authenticator.public_key_commitments().next().expect("key list has a key");

        authenticator
            .get_signature(commitment, &signing_inputs())
            .await
            .expect_err("a 64-byte signature is rejected");
    }
}

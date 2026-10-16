//! Seals the private transaction inputs sent alongside a submission, and adapts the transaction
//! encryption types to the node's protobuf and RPC errors.
//!
//! Transaction inputs are submitted as an IES-sealed blob rather than in the clear, so that the RPC
//! operator cannot read them and only holders of the validator set's shared encryption secret can.
//! Sealing uses the `X25519XChaCha20Poly1305` scheme; the sealed blob on the wire is a serialized
//! [`SealedMessage`](miden_protocol::crypto::ies::SealedMessage). The node rejects a submission
//! whose inputs are not sealed.

use alloc::string::ToString;
use alloc::vec::Vec;

pub use miden_client_core::errors::TransactionEncryptionError;
pub use miden_client_core::rpc::encryption::*;
use miden_protocol::Word;
use miden_protocol::transaction::{TransactionId, TransactionInputs};
use miden_protocol::utils::serde::Serializable;
use rand::CryptoRng;

use super::{RpcError, generated as proto};

// CONSTANTS
// ================================================================================================

/// Domain tag prefixed to the associated data of sealed transaction inputs.
///
/// Separates this transcript from every other use of the same key material, in particular from the
/// key attestation signed with the validator's signing key. Must match the validator's
/// `TX_INPUT_SEAL_DOMAIN`.
const TX_INPUT_SEAL_DOMAIN: &[u8] = b"MIDEN_TX_INPUT_SEAL_V1";

// ASSOCIATED DATA
// ================================================================================================

/// Builds the associated data authenticating a sealed set of transaction inputs.
///
/// Mirrors the validator's `transaction_inputs_associated_data`. The layout is
/// `TX_INPUT_SEAL_DOMAIN || scheme || len(key_id) || key_id || genesis_commitment ||
/// transaction_id`, where the scheme and the length prefix are 4 bytes little-endian. The domain
/// tag and the scheme are fixed-width, `key_id` is length-prefixed, and the two trailing fields are
/// a fixed 32 bytes each, so no two distinct inputs produce the same transcript.
///
/// Each binding serves a purpose:
/// - `scheme` and `key_id` tie the blob to one key, so inputs sealed against a retired key fail to
///   authenticate rather than silently decrypting.
/// - `genesis_commitment` ties the blob to one network. This matters in practice because every
///   development stack shares the same insecure default key, so without it a blob captured on one
///   network would replay onto another.
/// - `transaction_id` ties the blob to one transaction, so a captured blob cannot be replayed onto
///   a different transaction.
///
/// Deliberately absent is the serialized transaction. The RPC rebuilds the proven transaction with
/// output-note decorators stripped before forwarding a submission, so binding those bytes would
/// reject every relayed transaction. The transaction id is invariant under that rebuild, which is
/// why it is bound instead.
fn transaction_inputs_associated_data(
    scheme: u32,
    key_id: &[u8],
    genesis_commitment: Word,
    tx_id: TransactionId,
) -> Vec<u8> {
    let genesis_commitment = genesis_commitment.to_bytes();
    let tx_id = tx_id.as_word().to_bytes();
    let mut transcript = Vec::with_capacity(
        TX_INPUT_SEAL_DOMAIN.len()
            + 2 * size_of::<u32>()
            + key_id.len()
            + genesis_commitment.len()
            + tx_id.len(),
    );
    transcript.extend_from_slice(TX_INPUT_SEAL_DOMAIN);
    transcript.extend_from_slice(&scheme.to_le_bytes());
    extend_with_length_prefixed(&mut transcript, key_id);
    transcript.extend_from_slice(&genesis_commitment);
    transcript.extend_from_slice(&tx_id);

    transcript
}

/// Builds the associated data authenticating the inputs of the transaction identified by `tx_id`
/// when sealed against `key`.
fn key_associated_data(key: &TransactionEncryptionKey, tx_id: TransactionId) -> Vec<u8> {
    transaction_inputs_associated_data(key.scheme(), key.key_id(), key.genesis_commitment(), tx_id)
}

/// Appends a field prefixed with its length as 4 bytes little-endian.
fn extend_with_length_prefixed(payload: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).unwrap_or(u32::MAX);
    payload.extend_from_slice(&len.to_le_bytes());
    payload.extend_from_slice(field);
}

// SEALED TRANSACTION INPUTS
// ================================================================================================

/// The sealed, wire-ready form of a transaction's [`TransactionInputs`].
///
/// Wraps the serialized bytes of a [`SealedMessage`](miden_protocol::crypto::ies::SealedMessage) so
/// that a plaintext blob cannot be passed to submission by mistake, alongside the identifier of the
/// key they were sealed against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedTransactionInputs {
    key_id: Vec<u8>,
    ciphertext: Vec<u8>,
}

impl SealedTransactionInputs {
    /// Returns the identifier of the key these inputs were sealed against.
    pub fn key_id(&self) -> &[u8] {
        &self.key_id
    }

    /// Returns the sealed bytes.
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }
}

impl From<SealedTransactionInputs> for proto::submission::SealedTransactionInputs {
    fn from(sealed: SealedTransactionInputs) -> Self {
        Self {
            key_id: sealed.key_id().to_vec(),
            ciphertext: sealed.ciphertext().to_vec(),
        }
    }
}

// SEALING
// ================================================================================================

/// Seals the inputs of the transaction identified by `tx_id` against `key`, ready to be submitted.
///
/// `rng` supplies the scheme's ephemeral key material, so it must be cryptographically secure. Each
/// call draws a fresh ephemeral key, so sealing the same inputs twice is safe and yields different
/// ciphertexts.
pub fn seal_transaction_inputs<R: CryptoRng>(
    rng: &mut R,
    key: &TransactionEncryptionKey,
    tx_id: TransactionId,
    transaction_inputs: &TransactionInputs,
) -> Result<SealedTransactionInputs, RpcError> {
    let associated_data = key_associated_data(key, tx_id);
    let sealed = key
        .sealing_key()
        .seal_bytes_with_associated_data(rng, &transaction_inputs.to_bytes(), &associated_data)
        .map_err(|err| RpcError::TransactionInputsSealingFailed(err.to_string()))?;

    Ok(SealedTransactionInputs {
        key_id: key.key_id().to_vec(),
        ciphertext: sealed.to_bytes(),
    })
}

// ERRORS
// ================================================================================================

impl From<TransactionEncryptionError> for RpcError {
    fn from(err: TransactionEncryptionError) -> Self {
        match err {
            TransactionEncryptionError::KeyRejected(reason) => {
                RpcError::TransactionEncryptionKeyRejected(reason)
            },
        }
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::dsa::eddsa_25519_sha512::KeyExchangeKey;
    use miden_protocol::crypto::ies::{SealedMessage, UnsealingKey};
    use miden_protocol::utils::serde::Deserializable;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    use super::*;

    const TEST_KEY_ID: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

    #[test]
    fn supported_scheme_matches_the_node_protobuf() {
        assert_eq!(SUPPORTED_SCHEME, proto::submission::IesScheme::X25519Xchacha20Poly1305 as u32);
    }

    fn rng() -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(0xface)
    }

    fn genesis() -> Word {
        Word::from([1u32, 2, 3, 4])
    }

    fn tx_id(seed: u32) -> TransactionId {
        TransactionId::new(
            Word::from([seed, 0, 0, 0]),
            Word::from([0, seed, 0, 0]),
            Word::from([0, 0, seed, 0]),
            Word::from([0, 0, 0, seed]),
        )
    }

    /// Generates a keypair standing in for the validator set's shared key: the public half becomes
    /// the client's [`TransactionEncryptionKey`], the secret half plays the validator unsealing it.
    fn key_pair() -> (TransactionEncryptionKey, UnsealingKey) {
        let secret_key = KeyExchangeKey::with_rng(&mut rng());
        let key = TransactionEncryptionKey::new_unattested(
            TEST_KEY_ID.to_vec(),
            secret_key.public_key(),
            genesis(),
        );

        (key, UnsealingKey::X25519XChaCha20Poly1305(secret_key))
    }

    /// Unseals the way the validator does: rebuilding the associated data from its own view of the
    /// key and of the transaction rather than from anything the blob carries.
    fn unseal(
        unsealing_key: &UnsealingKey,
        sealed: &SealedTransactionInputs,
        key: &TransactionEncryptionKey,
        tx_id: TransactionId,
    ) -> Result<Vec<u8>, ()> {
        let associated_data = key_associated_data(key, tx_id);

        unsealing_key
            .unseal_bytes_with_associated_data(
                SealedMessage::read_from_bytes(sealed.ciphertext()).unwrap(),
                &associated_data,
            )
            .map_err(|_| ())
    }

    fn seal(key: &TransactionEncryptionKey, tx_id: TransactionId) -> SealedTransactionInputs {
        let associated_data = key_associated_data(key, tx_id);
        let sealed = key
            .sealing_key()
            .seal_bytes_with_associated_data(&mut rng(), b"transaction inputs", &associated_data)
            .unwrap();

        SealedTransactionInputs {
            key_id: key.key_id().to_vec(),
            ciphertext: sealed.to_bytes(),
        }
    }

    // ASSOCIATED DATA
    // --------------------------------------------------------------------------------------------

    /// Pins the transcript byte-for-byte, which also pins *which* fields it binds.
    ///
    /// Both sides derive the transcript through their own copy of this function, so a change to it
    /// would pass every other test in the workspace and surface only as every submission on the
    /// network failing to authenticate. This vector is the only thing that catches that, so it is
    /// spelled out here rather than derived from the constants it is checking.
    #[test]
    fn associated_data_matches_the_validator_transcript() {
        let associated_data =
            transaction_inputs_associated_data(1, &TEST_KEY_ID, genesis(), tx_id(10));

        let mut expected = Vec::new();
        expected.extend_from_slice(b"MIDEN_TX_INPUT_SEAL_V1");
        expected.extend_from_slice(&1u32.to_le_bytes());
        expected.extend_from_slice(&4u32.to_le_bytes());
        expected.extend_from_slice(&TEST_KEY_ID);
        expected.extend_from_slice(&genesis().to_bytes());
        expected.extend_from_slice(&tx_id(10).as_word().to_bytes());

        assert_eq!(associated_data, expected);
        // 22-byte tag + 4 scheme + 4 length + 4 key id + 32 genesis + 32 transaction id.
        assert_eq!(associated_data.len(), 98);
    }

    // SEALING
    // --------------------------------------------------------------------------------------------

    #[test]
    fn sealed_inputs_round_trip() {
        let (key, unsealing_key) = key_pair();
        let sealed = seal(&key, tx_id(10));

        assert_eq!(sealed.key_id(), key.key_id());
        let opened = unseal(&unsealing_key, &sealed, &key, tx_id(10)).unwrap();
        assert_eq!(opened, b"transaction inputs");
    }

    /// The transaction id binding: a blob captured from one submission must not authenticate when
    /// replayed onto a different transaction.
    #[test]
    fn unsealing_rejects_a_different_transaction() {
        let (key, unsealing_key) = key_pair();
        let sealed = seal(&key, tx_id(10));

        assert!(unseal(&unsealing_key, &sealed, &key, tx_id(11)).is_err());
    }

    /// The key id binding: inputs sealed against a retired key fail to authenticate rather than
    /// silently decrypting under the current one.
    #[test]
    fn unsealing_rejects_a_different_key_id() {
        let (key, unsealing_key) = key_pair();
        let sealed = seal(&key, tx_id(10));

        let rotated = TransactionEncryptionKey::new_unattested(
            b"other".to_vec(),
            key.public_key().clone(),
            key.genesis_commitment(),
        );
        assert!(unseal(&unsealing_key, &sealed, &rotated, tx_id(10)).is_err());
    }

    /// Each seal draws a fresh ephemeral key, so resealing the same inputs for the same transaction
    /// must not produce a linkable blob. Both seals draw from one RNG, as consecutive submissions
    /// from a single client do.
    #[test]
    fn sealing_the_same_inputs_twice_yields_different_ciphertexts() {
        let (key, unsealing_key) = key_pair();
        let associated_data = key_associated_data(&key, tx_id(10));
        let mut rng = rng();
        let mut seal_once = || {
            key.sealing_key()
                .seal_bytes_with_associated_data(&mut rng, b"transaction inputs", &associated_data)
                .unwrap()
                .to_bytes()
        };

        let first = seal_once();
        let second = seal_once();

        assert_ne!(first, second);
        for ciphertext in [first, second] {
            let sealed = SealedTransactionInputs {
                key_id: key.key_id().to_vec(),
                ciphertext,
            };
            assert_eq!(
                unseal(&unsealing_key, &sealed, &key, tx_id(10)).unwrap(),
                b"transaction inputs"
            );
        }
    }
}

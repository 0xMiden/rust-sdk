//! Client-side encryption of the private transaction inputs sent alongside a submission.
//!
//! Transaction inputs are submitted as an IES-sealed blob rather than in the clear, so that the
//! RPC operator cannot read them and only holders of the validator set's shared encryption secret
//! can. Sealing uses the `X25519XChaCha20Poly1305` scheme; the sealed blob on the wire is a
//! serialized [`SealedMessage`](miden_protocol::crypto::ies::SealedMessage).
//!
//! The key is public data shared by the whole validator set, so it is cached in the store rather
//! than re-derived per submission. See [`TransactionEncryptionKey`].
//!
//! # Provisioning the key
//!
//! The key is not obtained by this module. The node serves it from its
//! `GetTransactionEncryptionKey` endpoint together with per-validator attestations, but that
//! endpoint is not reachable from the protocol version this crate pins, so for now the key must be
//! provisioned by the caller through [`crate::store::Store::set_transaction_encryption_key`].
//! Submission fails with `ClientError::MissingTransactionEncryptionKey` until it is.
//!
//! When that endpoint does become reachable, the fetched key **must not** be trusted without
//! verifying its validator attestations against a validator signing key committed in a block
//! header. The endpoint is served by the RPC operator, which is the party this encryption exists to
//! keep out; an unverified key lets that operator substitute its own and read every submission.
//!
//! The associated-data preimage is likewise not yet specified upstream, so
//! [`TRANSACTION_INPUTS_ASSOCIATED_DATA`] is a placeholder that must match the node exactly or the
//! validator rejects the submission while unsealing.

use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::crypto::dsa::eddsa_25519_sha512::PublicKey;
use miden_protocol::crypto::ies::SealingKey;
use miden_protocol::transaction::TransactionInputs;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};
use rand::CryptoRng;

use super::RpcError;

// CONSTANTS
// ================================================================================================

/// Key used to store the transaction encryption key in the settings table.
pub(crate) const TRANSACTION_ENCRYPTION_KEY_STORE_SETTING: &str = "transaction_encryption_key";

/// Associated data bound into the AEAD tag of sealed transaction inputs.
///
/// Authenticated but not encrypted, and verified by the validator while unsealing: a mismatch
/// fails decryption rather than yielding wrong plaintext.
pub const TRANSACTION_INPUTS_ASSOCIATED_DATA: &[u8] = b"MIDEN_TX_INPUTS_ENCRYPTION_V0";

// TRANSACTION ENCRYPTION KEY
// ================================================================================================

/// The validator set's public transaction encryption key.
///
/// Holds public key material only, and is shared by every validator in the set; the matching
/// secret never leaves the validators.
///
/// The IES scheme is not carried: the node serves exactly one, and a new scheme would change the
/// public key encoding along with it, so the scheme is checked where a key enters the client rather
/// than stored alongside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionEncryptionKey {
    key_id: Vec<u8>,
    public_key: Vec<u8>,
}

impl TransactionEncryptionKey {
    /// Constructs a key from the node's opaque identifier for it and its raw public key bytes.
    ///
    /// The identifier changes when the key rotates, which is what lets a cached key be recognized
    /// as stale.
    pub fn new(key_id: Vec<u8>, public_key: Vec<u8>) -> Self {
        Self { key_id, public_key }
    }

    /// Returns the node's opaque identifier for this key.
    pub fn key_id(&self) -> &[u8] {
        &self.key_id
    }

    /// Returns the raw public key bytes.
    pub fn public_key(&self) -> &[u8] {
        &self.public_key
    }

    /// Builds the sealing key used to encrypt transaction inputs against this key.
    ///
    /// # Errors
    /// Returns an error if the public key bytes do not decode.
    pub fn sealing_key(&self) -> Result<SealingKey, RpcError> {
        let public_key = PublicKey::read_from_bytes(&self.public_key)
            .map_err(|err| RpcError::TransactionInputsSealingFailed(err.to_string()))?;

        Ok(SealingKey::X25519XChaCha20Poly1305(public_key))
    }
}

impl Serializable for TransactionEncryptionKey {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write_usize(self.key_id.len());
        target.write_bytes(&self.key_id);
        target.write_usize(self.public_key.len());
        target.write_bytes(&self.public_key);
    }
}

impl Deserializable for TransactionEncryptionKey {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let key_id_len = source.read_usize()?;
        let key_id = source.read_vec(key_id_len)?;
        let public_key_len = source.read_usize()?;
        let public_key = source.read_vec(public_key_len)?;

        Ok(Self::new(key_id, public_key))
    }
}

// SEALED TRANSACTION INPUTS
// ================================================================================================

/// The sealed, wire-ready form of a transaction's [`TransactionInputs`].
///
/// Wraps the serialized bytes of a [`SealedMessage`](miden_protocol::crypto::ies::SealedMessage)
/// so that a plaintext blob cannot be passed to submission by mistake.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedTransactionInputs(Vec<u8>);

impl SealedTransactionInputs {
    /// Returns the sealed bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the wrapper and returns the sealed bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

// SEALING
// ================================================================================================

/// Seals `transaction_inputs` against `key`, ready to be submitted.
///
/// `rng` supplies the scheme's ephemeral key material, so it must be cryptographically secure.
pub fn seal_transaction_inputs<R: CryptoRng>(
    rng: &mut R,
    key: &TransactionEncryptionKey,
    transaction_inputs: &TransactionInputs,
) -> Result<SealedTransactionInputs, RpcError> {
    let sealed = key
        .sealing_key()?
        .seal_bytes_with_associated_data(
            rng,
            &transaction_inputs.to_bytes(),
            TRANSACTION_INPUTS_ASSOCIATED_DATA,
        )
        .map_err(|err| RpcError::TransactionInputsSealingFailed(err.to_string()))?;

    Ok(SealedTransactionInputs(sealed.to_bytes()))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::dsa::eddsa_25519_sha512::KeyExchangeKey;
    use miden_protocol::crypto::ies::{SealedMessage, UnsealingKey};
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    use super::*;

    fn rng() -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(0xface)
    }

    /// Generates a keypair standing in for the validator set's shared key: the public half becomes
    /// the client's [`TransactionEncryptionKey`], the secret half plays the validator unsealing it.
    fn key_pair() -> (TransactionEncryptionKey, UnsealingKey) {
        let secret_key = KeyExchangeKey::with_rng(&mut rng());
        let key =
            TransactionEncryptionKey::new(b"key-id".to_vec(), secret_key.public_key().to_bytes());

        (key, UnsealingKey::X25519XChaCha20Poly1305(secret_key))
    }

    fn seal(key: &TransactionEncryptionKey, associated_data: &[u8]) -> Vec<u8> {
        key.sealing_key()
            .unwrap()
            .seal_bytes_with_associated_data(&mut rng(), b"transaction inputs", associated_data)
            .unwrap()
            .to_bytes()
    }

    fn unseal(
        unsealing_key: &UnsealingKey,
        sealed: &[u8],
        associated_data: &[u8],
    ) -> Result<Vec<u8>, ()> {
        unsealing_key
            .unseal_bytes_with_associated_data(
                SealedMessage::read_from_bytes(sealed).unwrap(),
                associated_data,
            )
            .map_err(|_| ())
    }

    #[test]
    fn sealed_inputs_round_trip() {
        let (key, unsealing_key) = key_pair();
        let sealed = seal(&key, TRANSACTION_INPUTS_ASSOCIATED_DATA);

        let opened = unseal(&unsealing_key, &sealed, TRANSACTION_INPUTS_ASSOCIATED_DATA).unwrap();
        assert_eq!(opened, b"transaction inputs");
    }

    #[test]
    fn unsealing_rejects_mismatched_associated_data() {
        let (key, unsealing_key) = key_pair();
        let sealed = seal(&key, TRANSACTION_INPUTS_ASSOCIATED_DATA);

        assert!(unseal(&unsealing_key, &sealed, b"other associated data").is_err());
    }

    /// A key that survived serialization must still seal for the same secret, since it is cached in
    /// the settings table in this form.
    #[test]
    fn key_rebuilt_from_persisted_parts_still_seals() {
        let (key, unsealing_key) = key_pair();
        let restored = TransactionEncryptionKey::read_from_bytes(&key.to_bytes()).unwrap();
        assert_eq!(restored, key);

        let sealed = seal(&restored, TRANSACTION_INPUTS_ASSOCIATED_DATA);
        assert!(unseal(&unsealing_key, &sealed, TRANSACTION_INPUTS_ASSOCIATED_DATA).is_ok());
    }
}

//! Client-side encryption of the private transaction inputs sent alongside a submission.
//!
//! Transaction inputs are submitted as an IES-sealed blob rather than in the clear, so that the
//! RPC operator cannot read them and only holders of the validator set's shared encryption secret
//! can. Sealing uses the `X25519XChaCha20Poly1305` scheme; the sealed blob on the wire is a
//! serialized [`SealedMessage`](miden_protocol::crypto::ies::SealedMessage). The node rejects a
//! submission whose inputs are not sealed.
//!
//! # Trusting the key
//!
//! The key is served by the node's `GetTransactionEncryptionKey` endpoint, which the RPC operator
//! controls -- and that operator is the party this encryption exists to keep out. A key taken from
//! that endpoint on faith would let the operator substitute its own, decrypt every submission, and
//! re-seal under the real validator key undetected.
//!
//! So a fetched key is never used directly. [`AttestedTransactionEncryptionKey`] is the only thing
//! the RPC layer can produce, and the sole way to obtain a usable [`TransactionEncryptionKey`] from
//! it is [`AttestedTransactionEncryptionKey::verify`], which requires a validator signature over
//! [`attestation_commitment`] that checks out against a validator signing key committed in a block
//! header. The commitment binds the genesis commitment, so an attestation cannot be replayed from
//! another network sharing a validator key.
//!
//! Once verified, the key is public data shared by the whole validator set, so it is cached in the
//! store rather than re-fetched per submission. A submission rejected for having been sealed
//! against a key the validator no longer holds evicts the cached key, so the next submission
//! fetches and verifies a fresh one.
//!
//! # Matching the validator's transcripts
//!
//! The canonical definitions live in the node's `miden_node_proto::domain::encryption`. This module
//! is a hand-maintained mirror of them, because that is a node crate and this client is `no_std`.

use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::block::{BlockNumber, ValidatorKeys};
use miden_protocol::crypto::dsa::ecdsa_k256_keccak::{
    PublicKey as ValidatorPublicKey,
    Signature as ValidatorSignature,
};
use miden_protocol::crypto::dsa::eddsa_25519_sha512::PublicKey;
use miden_protocol::crypto::ies::SealingKey;
use miden_protocol::transaction::{TransactionId, TransactionInputs};
use miden_protocol::{Hasher, Word};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};
use rand::CryptoRng;

use super::generated::transaction::IesScheme;
use super::{RpcError, generated as proto};

// CONSTANTS
// ================================================================================================

/// Key used to store the transaction encryption key in the settings table.
pub(crate) const TRANSACTION_ENCRYPTION_KEY_STORE_SETTING: &str = "transaction_encryption_key";

/// Domain tag prefixed to the associated data of sealed transaction inputs.
///
/// Separates this transcript from every other use of the same key material, in particular from the
/// key attestation signed with the validator's signing key. Must match the validator's
/// `TX_INPUT_SEAL_DOMAIN`.
const TX_INPUT_SEAL_DOMAIN: &[u8] = b"MIDEN_TX_INPUT_SEAL_V1";

/// Domain tag prefixed to the attestation payload, separating key attestations from block header
/// signatures made with the same validator signing key.
///
/// Must match the validator's `ATTESTATION_DOMAIN`.
const ATTESTATION_DOMAIN: &[u8] = b"MIDEN_TX_ENCRYPTION_KEY_ATTESTATION_V1";

/// Wire identifier of the only IES scheme this client seals for.
const SUPPORTED_SCHEME: u32 = IesScheme::X25519Xchacha20Poly1305 as u32;

/// Longest key identifier accepted from the RPC, in bytes.
///
/// Must match the validator's `MAX_KEY_ID_LEN`.
const MAX_KEY_ID_LEN: usize = 64;

// TRANSACTION ENCRYPTION KEY
// ================================================================================================

/// The validator set's public transaction encryption key, with its attestation already verified.
///
/// Holds public key material only, and is shared by every validator in the set; the matching
/// secret never leaves the validators.
///
/// Only [`AttestedTransactionEncryptionKey::verify`] constructs one, so a key that reaches the seal
/// path has necessarily been vouched for by a chain-recognized validator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionEncryptionKey {
    scheme: u32,
    key_id: Vec<u8>,
    public_key: PublicKey,
    genesis_commitment: Word,
}

impl TransactionEncryptionKey {
    /// Returns the node's opaque identifier for this key.
    ///
    /// The identifier changes when the key rotates, which is what lets a cached key be recognized
    /// as stale. It is treated as opaque bytes: the node derives it from the public key commitment
    /// but documents the encoding as an implementation detail.
    pub fn key_id(&self) -> &[u8] {
        &self.key_id
    }

    /// Returns the public key.
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// Builds the associated data authenticating the inputs of the transaction identified by
    /// `tx_id` when sealed against this key.
    fn transaction_inputs_associated_data(&self, tx_id: TransactionId) -> Vec<u8> {
        transaction_inputs_associated_data(
            self.scheme,
            &self.key_id,
            self.genesis_commitment,
            tx_id,
        )
    }

    /// Builds the sealing key used to encrypt transaction inputs against this key.
    pub fn sealing_key(&self) -> SealingKey {
        SealingKey::X25519XChaCha20Poly1305(self.public_key.clone())
    }

    /// Builds a key without an attestation, for tests.
    ///
    /// Sealing against the returned key still runs the real transcript and wire path, only
    /// the attestation is skipped, and that is covered by this module's own tests.
    #[cfg(feature = "testing")]
    pub fn new_unattested(
        key_id: Vec<u8>,
        public_key: PublicKey,
        genesis_commitment: Word,
    ) -> Self {
        Self {
            scheme: SUPPORTED_SCHEME,
            key_id,
            public_key,
            genesis_commitment,
        }
    }
}

impl Serializable for TransactionEncryptionKey {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write_u32(self.scheme);
        target.write_usize(self.key_id.len());
        target.write_bytes(&self.key_id);
        self.public_key.write_into(target);
        self.genesis_commitment.write_into(target);
    }
}

impl Deserializable for TransactionEncryptionKey {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let scheme = source.read_u32()?;
        let key_id_len = source.read_usize()?;
        let key_id = source.read_vec(key_id_len)?;
        let public_key = PublicKey::read_from(source)?;
        let genesis_commitment = Word::read_from(source)?;

        Ok(Self {
            scheme,
            key_id,
            public_key,
            genesis_commitment,
        })
    }
}

// ATTESTED TRANSACTION ENCRYPTION KEY
// ================================================================================================

/// The next encryption key announced ahead of a scheduled rotation.
///
/// Covered by [`attestation_commitment`], so it cannot be stripped or altered without invalidating
/// the attestations. Carried for verification only; this client does not yet act on rotations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NextTransactionEncryptionKey {
    /// Wire identifier of the next key's IES scheme.
    pub scheme: u32,
    /// Opaque identifier of the next key.
    pub key_id: Vec<u8>,
    /// Raw public key bytes of the next key.
    pub public_key: Vec<u8>,
    /// Block number at which the next key takes effect.
    pub rotation_block_num: BlockNumber,
}

/// A single validator's endorsement of a served encryption key.
///
/// The signature covers [`attestation_commitment`] recomputed from the served fields, and counts
/// only if `validator_key` is present in a validator set committed in a block header this client
/// trusts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatorAttestation {
    /// Signing key of the attesting validator.
    pub validator_key: ValidatorPublicKey,
    /// The validator's signature over [`attestation_commitment`].
    pub signature: ValidatorSignature,
}

/// A transaction encryption key exactly as the node served it, before it is trusted.
///
/// Deliberately not usable for sealing. [`Self::verify`] is the only way to turn it into a
/// [`TransactionEncryptionKey`], so a key served by an untrusted RPC cannot reach the seal path
/// without a validator attestation checking out first.
///
/// Fields are kept in their served wire form because the attestation commitment is computed over
/// exactly those bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttestedTransactionEncryptionKey {
    /// Wire identifier of the key's IES scheme.
    pub scheme: u32,
    /// Opaque identifier of the key.
    pub key_id: Vec<u8>,
    /// Raw public key bytes.
    pub public_key: Vec<u8>,
    /// Validator attestations over [`attestation_commitment`].
    pub attestations: Vec<ValidatorAttestation>,
    /// The next key, when a rotation is scheduled.
    pub next_key: Option<NextTransactionEncryptionKey>,
}

impl AttestedTransactionEncryptionKey {
    /// Verifies the served key and returns it in usable form.
    ///
    /// Requires at least one attestation whose validator key is present in `validator_keys` -- the
    /// set committed in a block header this client trusts -- and whose signature covers the
    /// commitment recomputed from the served fields. Every validator vouches for the same key, so
    /// one verifiable attestation from a chain-recognized validator is sufficient.
    ///
    /// # Errors
    /// Returns an error if the scheme is unsupported, the public key does not decode, or no
    /// attestation from a recognized validator verifies.
    pub fn verify(
        self,
        genesis_commitment: Word,
        validator_keys: &ValidatorKeys,
    ) -> Result<TransactionEncryptionKey, RpcError> {
        if self.scheme != SUPPORTED_SCHEME {
            return Err(RpcError::TransactionEncryptionKeyRejected(format!(
                "unsupported IES scheme '{}'",
                self.scheme
            )));
        }

        validate_key_id(&self.key_id, "encryption key id")?;
        if let Some(next) = &self.next_key {
            validate_key_id(&next.key_id, "next encryption key id")?;
        }

        let commitment = attestation_commitment(
            self.scheme,
            &self.key_id,
            genesis_commitment,
            &self.public_key,
            self.next_key.as_ref(),
        );

        let recognized = validator_keys.as_keys();
        let attested = self.attestations.iter().any(|attestation| {
            recognized.contains(&attestation.validator_key)
                && attestation.validator_key.verify(commitment, &attestation.signature)
        });
        if !attested {
            return Err(RpcError::TransactionEncryptionKeyRejected(
                "no attestation from a chain-recognized validator verifies against the key".into(),
            ));
        }

        // Parsed after verification: the commitment covers the served bytes, so decoding earlier
        // would accept a shape the attestation never signed.
        let public_key = PublicKey::read_from_bytes(&self.public_key)
            .map_err(|err| RpcError::TransactionEncryptionKeyRejected(err.to_string()))?;

        Ok(TransactionEncryptionKey {
            scheme: self.scheme,
            key_id: self.key_id,
            public_key,
            genesis_commitment,
        })
    }
}

/// Rejects a served key identifier that is empty or longer than [`MAX_KEY_ID_LEN`].
///
/// Mirrors the validator's `validate_key_id` so the client refuses a key the node itself
/// would never serve.
fn validate_key_id(key_id: &[u8], field: &str) -> Result<(), RpcError> {
    if key_id.is_empty() {
        return Err(RpcError::TransactionEncryptionKeyRejected(format!("{field} is empty")));
    }
    if key_id.len() > MAX_KEY_ID_LEN {
        return Err(RpcError::TransactionEncryptionKeyRejected(format!(
            "{field} is {} bytes, which exceeds the maximum of {MAX_KEY_ID_LEN}",
            key_id.len()
        )));
    }
    Ok(())
}

/// Computes the commitment a validator signs to attest an encryption key.
///
/// Mirrors the validator's `attestation_commitment` (`signers::attestation_commitment` in the
/// `miden-validator` crate of `0xMiden/node`) so the layout is duplicated here and pinned against
/// the validator's output by the golden-vector tests below: the Poseidon2 hash of
/// `ATTESTATION_DOMAIN || scheme || len(key_id) || key_id || genesis_commitment || len(public_key)
/// || public_key || next_key_transcript`, where the scheme, rotation block number and length
/// prefixes are 4 bytes little-endian. The length prefixes keep the payload injective, and the
/// genesis commitment ties the attestation to one chain. Any divergence from the validator's layout
/// makes every signature fail to verify.
pub fn attestation_commitment(
    scheme: u32,
    key_id: &[u8],
    genesis_commitment: Word,
    public_key: &[u8],
    next_key: Option<&NextTransactionEncryptionKey>,
) -> Word {
    let mut payload = Vec::new();
    payload.extend_from_slice(ATTESTATION_DOMAIN);
    payload.extend_from_slice(&scheme.to_le_bytes());
    extend_with_length_prefixed(&mut payload, key_id);
    payload.extend_from_slice(&genesis_commitment.to_bytes());
    extend_with_length_prefixed(&mut payload, public_key);
    if let Some(next) = next_key {
        payload.extend_from_slice(&next.scheme.to_le_bytes());
        extend_with_length_prefixed(&mut payload, &next.key_id);
        extend_with_length_prefixed(&mut payload, &next.public_key);
        payload.extend_from_slice(&next.rotation_block_num.as_u32().to_le_bytes());
    }

    Hasher::hash(&payload)
}

/// Appends a field prefixed with its length as 4 bytes little-endian.
///
/// A field longer than `u32::MAX` cannot occur in a response this client accepts, and saturating
/// keeps the helper infallible; an inaccurate prefix only makes verification fail.
fn extend_with_length_prefixed(payload: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).unwrap_or(u32::MAX);
    payload.extend_from_slice(&len.to_le_bytes());
    payload.extend_from_slice(field);
}

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

// SEALED TRANSACTION INPUTS
// ================================================================================================

/// The sealed, wire-ready form of a transaction's [`TransactionInputs`].
///
/// Wraps the serialized bytes of a [`SealedMessage`](miden_protocol::crypto::ies::SealedMessage)
/// so that a plaintext blob cannot be passed to submission by mistake, alongside the identifier of
/// the key they were sealed against.
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

impl From<SealedTransactionInputs> for proto::transaction::SealedTransactionInputs {
    fn from(sealed: SealedTransactionInputs) -> Self {
        Self {
            key_id: sealed.key_id,
            ciphertext: sealed.ciphertext,
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
    let associated_data = key.transaction_inputs_associated_data(tx_id);
    let sealed = key
        .sealing_key()
        .seal_bytes_with_associated_data(rng, &transaction_inputs.to_bytes(), &associated_data)
        .map_err(|err| RpcError::TransactionInputsSealingFailed(err.to_string()))?;

    Ok(SealedTransactionInputs {
        key_id: key.key_id().to_vec(),
        ciphertext: sealed.to_bytes(),
    })
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::dsa::ecdsa_k256_keccak::SigningKey as ValidatorSigningKey;
    use miden_protocol::crypto::dsa::eddsa_25519_sha512::KeyExchangeKey;
    use miden_protocol::crypto::ies::{SealedMessage, UnsealingKey};
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    use super::*;

    const TEST_KEY_ID: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

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
        let key = TransactionEncryptionKey {
            scheme: SUPPORTED_SCHEME,
            key_id: TEST_KEY_ID.to_vec(),
            public_key: secret_key.public_key(),
            genesis_commitment: genesis(),
        };

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
        let associated_data = key.transaction_inputs_associated_data(tx_id);

        unsealing_key
            .unseal_bytes_with_associated_data(
                SealedMessage::read_from_bytes(sealed.ciphertext()).unwrap(),
                &associated_data,
            )
            .map_err(|_| ())
    }

    fn seal(key: &TransactionEncryptionKey, tx_id: TransactionId) -> SealedTransactionInputs {
        let associated_data = key.transaction_inputs_associated_data(tx_id);
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

        let rotated = TransactionEncryptionKey { key_id: b"other".to_vec(), ..key };
        assert!(unseal(&unsealing_key, &sealed, &rotated, tx_id(10)).is_err());
    }

    /// Each seal draws a fresh ephemeral key, so resealing the same inputs for the same transaction
    /// must not produce a linkable blob. Both seals draw from one RNG, as consecutive submissions
    /// from a single client do.
    #[test]
    fn sealing_the_same_inputs_twice_yields_different_ciphertexts() {
        let (key, unsealing_key) = key_pair();
        let associated_data = key.transaction_inputs_associated_data(tx_id(10));
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

    // ATTESTATION VERIFICATION
    // --------------------------------------------------------------------------------------------

    /// Builds a response attested by `signer`, the way a validator serves one.
    fn attested(
        key: &TransactionEncryptionKey,
        signer: &ValidatorSigningKey,
        genesis_commitment: Word,
    ) -> AttestedTransactionEncryptionKey {
        attested_with_next(key, signer, genesis_commitment, None)
    }

    /// Builds a response whose signature also covers `next_key`, so that tests exercising a
    /// scheduled rotation fail for the reason they name rather than for an invalid signature.
    fn attested_with_next(
        key: &TransactionEncryptionKey,
        signer: &ValidatorSigningKey,
        genesis_commitment: Word,
        next_key: Option<NextTransactionEncryptionKey>,
    ) -> AttestedTransactionEncryptionKey {
        let public_key = key.public_key().to_bytes();
        let commitment = attestation_commitment(
            SUPPORTED_SCHEME,
            key.key_id(),
            genesis_commitment,
            &public_key,
            next_key.as_ref(),
        );

        AttestedTransactionEncryptionKey {
            scheme: SUPPORTED_SCHEME,
            key_id: key.key_id().to_vec(),
            public_key,
            attestations: vec![ValidatorAttestation {
                validator_key: signer.public_key(),
                signature: signer.sign(commitment),
            }],
            next_key,
        }
    }

    #[test]
    fn verify_accepts_an_attestation_from_a_recognized_validator() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let verified =
            attested(&key, &signer, genesis()).verify(genesis(), &validator_keys).unwrap();

        assert_eq!(verified, key);
    }

    #[test]
    fn verify_rejects_a_validator_absent_from_the_committed_set() {
        let (key, _) = key_pair();
        let impostor = ValidatorSigningKey::with_rng(&mut rng());
        let committed = ValidatorSigningKey::with_rng(&mut ChaCha20Rng::seed_from_u64(7));
        let validator_keys = ValidatorKeys::new(vec![committed.public_key()]).unwrap();

        assert!(attested(&key, &impostor, genesis()).verify(genesis(), &validator_keys).is_err());
    }

    /// The whole point of the attestation: a substituted public key must not verify, even though
    /// the signature itself is genuine.
    #[test]
    fn verify_rejects_a_substituted_public_key() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let substitute = KeyExchangeKey::with_rng(&mut ChaCha20Rng::seed_from_u64(99));
        let mut response = attested(&key, &signer, genesis());
        response.public_key = substitute.public_key().to_bytes();

        assert!(response.verify(genesis(), &validator_keys).is_err());
    }

    /// The genesis commitment scopes an attestation to one chain, so the same signed response must
    /// not verify against a different network.
    #[test]
    fn verify_rejects_an_attestation_from_another_network() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let response = attested(&key, &signer, genesis());

        assert!(response.verify(Word::from([9u32, 9, 9, 9]), &validator_keys).is_err());
    }

    /// A scheduled rotation is covered by the signature, so it cannot be injected or altered by the
    /// operator relaying the response.
    #[test]
    fn verify_rejects_an_injected_next_key() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let mut response = attested(&key, &signer, genesis());
        response.next_key = Some(NextTransactionEncryptionKey {
            scheme: SUPPORTED_SCHEME,
            key_id: vec![1, 2, 3, 4],
            public_key: KeyExchangeKey::with_rng(&mut ChaCha20Rng::seed_from_u64(11))
                .public_key()
                .to_bytes(),
            rotation_block_num: 100.into(),
        });

        assert!(response.verify(genesis(), &validator_keys).is_err());
    }

    #[test]
    fn verify_rejects_an_unsupported_scheme() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let mut response = attested(&key, &signer, genesis());
        response.scheme = SUPPORTED_SCHEME + 1;

        assert!(response.verify(genesis(), &validator_keys).is_err());
    }

    // VALIDATOR PARITY
    // --------------------------------------------------------------------------------------------

    /// Expected values produced by the validator's own implementation over these exact inputs
    /// (`miden_validator::attestation_commitment`, `0xMiden/node` rev `5066b383`, identical on
    /// `next` at `da261511`). The commitment layout is duplicated on both sides, so these vectors
    /// are what ties them together: if either side changes its layout, this test fails rather
    /// than every attestation quietly failing to verify. Regenerate by feeding the same inputs to
    /// the node's function.
    #[test]
    fn attestation_commitment_matches_the_validator_implementation() {
        let genesis = Word::from([101u32, 102, 103, 104]);

        let no_rotation =
            attestation_commitment(1, b"golden-key-id", genesis, b"golden-public-key", None);
        assert_eq!(
            no_rotation.to_hex(),
            "0x245d1f2d45d4a60d9edd4576691244d6b9ee16fe67635425dc685cd54918a970"
        );

        let next = NextTransactionEncryptionKey {
            scheme: 2,
            key_id: b"next-key-id".to_vec(),
            public_key: b"next-public-key".to_vec(),
            rotation_block_num: BlockNumber::from(7u32),
        };
        let with_rotation =
            attestation_commitment(1, b"golden-key-id", genesis, b"golden-public-key", Some(&next));
        assert_eq!(
            with_rotation.to_hex(),
            "0xddfd7907b6a1ea6f294809ff0ed775f270b649ca15b21f88127c8335945e4752"
        );
    }
}

//! The validator set's transaction encryption key, which the client uses to seal the private
//! transaction inputs sent alongside a submission.
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

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use miden_protocol::block::{BlockNumber, ValidatorConfig};
use miden_protocol::crypto::dsa::ecdsa_k256_keccak::{
    PublicKey as ValidatorPublicKey,
    Signature as ValidatorSignature,
};
use miden_protocol::crypto::dsa::eddsa_25519_sha512::PublicKey;
use miden_protocol::crypto::ies::SealingKey;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Hasher, Word};

use crate::errors::TransactionEncryptionError;

// CONSTANTS
// ================================================================================================

/// Domain tag prefixed to the attestation payload, separating key attestations from block header
/// signatures made with the same validator signing key.
///
/// Must match the validator's `ATTESTATION_DOMAIN`.
const ATTESTATION_DOMAIN: &[u8] = b"MIDEN_TX_ENCRYPTION_KEY_ATTESTATION_V1";

/// Wire identifier of the only IES scheme this client seals for.
///
/// Must match `IesScheme::X25519Xchacha20Poly1305` in the node's protobuf.
pub const SUPPORTED_SCHEME: u32 = 1;

/// Longest key identifier accepted from the RPC, in bytes.
///
/// Must match the validator's `MAX_KEY_ID_LEN`.
const MAX_KEY_ID_LEN: usize = 64;

// TRANSACTION ENCRYPTION KEY
// ================================================================================================

/// The validator set's public transaction encryption key, with its attestation already verified.
///
/// Holds public key material only, and is shared by every validator in the set; the matching secret
/// never leaves the validators.
///
/// RPC responses can only construct one through [`AttestedTransactionEncryptionKey::verify`].
/// [`Self::from_parts`] revalidates the scheme and key ID of cached keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransactionEncryptionKey {
    scheme: u32,
    key_id: Vec<u8>,
    public_key: PublicKey,
    genesis_commitment: Word,
}

impl TransactionEncryptionKey {
    /// Builds a cached key from its parts. A store uses this to rebuild a key that it persisted in
    /// its own format.
    ///
    /// The parts are not attested. The constructor checks the scheme and the key ID length.
    pub fn from_parts(
        scheme: u32,
        key_id: Vec<u8>,
        public_key: PublicKey,
        genesis_commitment: Word,
    ) -> Result<Self, String> {
        validate_key_metadata(scheme, key_id.len())?;
        Ok(Self {
            scheme,
            key_id,
            public_key,
            genesis_commitment,
        })
    }

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

    /// Returns the wire identifier of the key's IES scheme.
    pub fn scheme(&self) -> u32 {
        self.scheme
    }

    /// Returns the commitment of the genesis block of the network that attested this key.
    pub fn genesis_commitment(&self) -> Word {
        self.genesis_commitment
    }

    /// Builds the sealing key used to encrypt transaction inputs against this key.
    pub fn sealing_key(&self) -> SealingKey {
        SealingKey::X25519XChaCha20Poly1305(self.public_key.clone())
    }

    /// Builds a key without an attestation, for tests.
    ///
    /// Sealing against the returned key still runs the real transcript and wire path, only the
    /// attestation is skipped, and that is covered by this module's own tests.
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
        validator_keys: &ValidatorConfig,
    ) -> Result<TransactionEncryptionKey, TransactionEncryptionError> {
        validate_key_metadata(self.scheme, self.key_id.len())
            .map_err(TransactionEncryptionError::KeyRejected)?;
        if let Some(next) = &self.next_key {
            validate_key_id_len(next.key_id.len(), "next encryption key id")
                .map_err(TransactionEncryptionError::KeyRejected)?;
        }

        let commitment = attestation_commitment(
            self.scheme,
            &self.key_id,
            genesis_commitment,
            &self.public_key,
            self.next_key.as_ref(),
        );

        let recognized = validator_keys.keys();
        let attested = self.attestations.iter().any(|attestation| {
            recognized.contains(&attestation.validator_key)
                && attestation.validator_key.verify(commitment, &attestation.signature)
        });
        if !attested {
            return Err(TransactionEncryptionError::KeyRejected(
                "no attestation from a chain-recognized validator verifies against the key".into(),
            ));
        }

        // Parsed after verification: the commitment covers the served bytes, so decoding earlier
        // would accept a shape the attestation never signed.
        let public_key = PublicKey::read_from_bytes(&self.public_key)
            .map_err(|err| TransactionEncryptionError::KeyRejected(err.to_string()))?;

        Ok(TransactionEncryptionKey {
            scheme: self.scheme,
            key_id: self.key_id,
            public_key,
            genesis_commitment,
        })
    }
}

fn validate_key_metadata(scheme: u32, key_id_len: usize) -> Result<(), String> {
    if scheme != SUPPORTED_SCHEME {
        return Err(format!("unsupported IES scheme '{scheme}'"));
    }

    validate_key_id_len(key_id_len, "encryption key id")
}

fn validate_key_id_len(key_id_len: usize, field: &str) -> Result<(), String> {
    if key_id_len == 0 {
        return Err(format!("{field} is empty"));
    }
    if key_id_len > MAX_KEY_ID_LEN {
        return Err(format!(
            "{field} is {key_id_len} bytes, which exceeds the maximum of {MAX_KEY_ID_LEN}"
        ));
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

// SEALED TRANSACTION INPUTS
// ================================================================================================

/// The sealed, wire-ready form of a transaction's
/// [`TransactionInputs`](miden_protocol::transaction::TransactionInputs).
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
    /// Creates a new [`SealedTransactionInputs`] from sealed bytes and the identifier of the key
    /// they were sealed against.
    pub fn new(key_id: Vec<u8>, ciphertext: Vec<u8>) -> Self {
        Self { key_id, ciphertext }
    }

    /// Returns the identifier of the key these inputs were sealed against.
    pub fn key_id(&self) -> &[u8] {
        &self.key_id
    }

    /// Returns the sealed bytes.
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::crypto::dsa::ecdsa_k256_keccak::SigningKey as ValidatorSigningKey;
    use miden_protocol::crypto::dsa::eddsa_25519_sha512::KeyExchangeKey;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    use super::*;

    const TEST_KEY_ID: [u8; 4] = [0xde, 0xad, 0xbe, 0xef];

    #[test]
    fn transaction_encryption_key_from_parts_validates_metadata() {
        let key = test_key();
        let from_parts = |key: TransactionEncryptionKey| {
            TransactionEncryptionKey::from_parts(
                key.scheme,
                key.key_id,
                key.public_key,
                key.genesis_commitment,
            )
        };
        assert_eq!(from_parts(key.clone()).unwrap(), key);

        let invalid_keys = [
            TransactionEncryptionKey {
                scheme: SUPPORTED_SCHEME + 1,
                ..key.clone()
            },
            TransactionEncryptionKey { key_id: Vec::new(), ..key.clone() },
            TransactionEncryptionKey {
                key_id: vec![0; MAX_KEY_ID_LEN + 1],
                ..key
            },
        ];
        for key in invalid_keys {
            assert!(from_parts(key).is_err());
        }
    }

    fn rng() -> ChaCha20Rng {
        ChaCha20Rng::seed_from_u64(0xface)
    }

    fn genesis() -> Word {
        Word::from([1u32, 2, 3, 4])
    }

    /// Builds a key standing in for the validator set's shared key.
    fn test_key() -> TransactionEncryptionKey {
        TransactionEncryptionKey {
            scheme: SUPPORTED_SCHEME,
            key_id: TEST_KEY_ID.to_vec(),
            public_key: KeyExchangeKey::with_rng(&mut rng()).public_key(),
            genesis_commitment: genesis(),
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
        let key = test_key();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorConfig::new(vec![signer.public_key()], 1).unwrap();

        let verified =
            attested(&key, &signer, genesis()).verify(genesis(), &validator_keys).unwrap();

        assert_eq!(verified, key);
    }

    #[test]
    fn verify_rejects_a_validator_absent_from_the_committed_set() {
        let key = test_key();
        let impostor = ValidatorSigningKey::with_rng(&mut rng());
        let committed = ValidatorSigningKey::with_rng(&mut ChaCha20Rng::seed_from_u64(7));
        let validator_keys = ValidatorConfig::new(vec![committed.public_key()], 1).unwrap();

        assert!(attested(&key, &impostor, genesis()).verify(genesis(), &validator_keys).is_err());
    }

    /// The whole point of the attestation: a substituted public key must not verify, even though
    /// the signature itself is genuine.
    #[test]
    fn verify_rejects_a_substituted_public_key() {
        let key = test_key();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorConfig::new(vec![signer.public_key()], 1).unwrap();

        let substitute = KeyExchangeKey::with_rng(&mut ChaCha20Rng::seed_from_u64(99));
        let mut response = attested(&key, &signer, genesis());
        response.public_key = substitute.public_key().to_bytes();

        assert!(response.verify(genesis(), &validator_keys).is_err());
    }

    /// The genesis commitment scopes an attestation to one chain, so the same signed response must
    /// not verify against a different network.
    #[test]
    fn verify_rejects_an_attestation_from_another_network() {
        let key = test_key();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorConfig::new(vec![signer.public_key()], 1).unwrap();

        let response = attested(&key, &signer, genesis());

        assert!(response.verify(Word::from([9u32, 9, 9, 9]), &validator_keys).is_err());
    }

    /// A scheduled rotation is covered by the signature, so it cannot be injected or altered by the
    /// operator relaying the response.
    #[test]
    fn verify_rejects_an_injected_next_key() {
        let key = test_key();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorConfig::new(vec![signer.public_key()], 1).unwrap();

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
        let key = test_key();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorConfig::new(vec![signer.public_key()], 1).unwrap();

        let mut response = attested(&key, &signer, genesis());
        response.scheme = SUPPORTED_SCHEME + 1;

        assert!(response.verify(genesis(), &validator_keys).is_err());
    }

    // VALIDATOR PARITY
    // --------------------------------------------------------------------------------------------

    /// Expected values produced by the validator's own implementation over these exact inputs
    /// (`miden_validator::attestation_commitment`, `0xMiden/node` rev `5066b383`, identical on
    /// `next` at `da261511`). The commitment layout is duplicated on both sides, so these vectors
    /// are what ties them together: if either side changes its layout, this test fails rather than
    /// every attestation quietly failing to verify. Regenerate by feeding the same inputs to the
    /// node's function.
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

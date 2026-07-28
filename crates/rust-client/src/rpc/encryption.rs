//! Client-side encryption of the private transaction inputs sent alongside a submission.
//!
//! Transaction inputs are submitted as an IES-sealed blob rather than in the clear, so that the
//! RPC operator cannot read them and only holders of the validator set's shared encryption secret
//! can. Sealing uses the `X25519XChaCha20Poly1305` scheme; the sealed blob on the wire is a
//! serialized [`SealedMessage`](miden_protocol::crypto::ies::SealedMessage).
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
//! store rather than re-fetched per submission.
//!
//! # Provisional associated data
//!
//! The associated-data preimage is not yet specified upstream, so
//! [`TRANSACTION_INPUTS_ASSOCIATED_DATA`] is a placeholder that must match the node exactly or the
//! validator rejects the submission while unsealing.

use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::block::ValidatorKeys;
use miden_protocol::crypto::dsa::ecdsa_k256_keccak::{
    PublicKey as ValidatorPublicKey,
    Signature as ValidatorSignature,
};
use miden_protocol::crypto::dsa::eddsa_25519_sha512::PublicKey;
use miden_protocol::crypto::ies::SealingKey;
use miden_protocol::transaction::TransactionInputs;
use miden_protocol::{Hasher, Word};
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

/// Domain tag prefixed to the attestation payload, separating key attestations from block header
/// signatures made with the same validator signing key.
///
/// Must match the validator's `ATTESTATION_DOMAIN`.
const ATTESTATION_DOMAIN: &[u8] = b"MIDEN_TX_ENCRYPTION_KEY_ATTESTATION_V1";

/// Wire identifier of the only IES scheme this client seals for,
/// `IES_SCHEME_X25519_XCHACHA20_POLY1305`.
const SUPPORTED_SCHEME: u32 = 1;

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
    public_key: PublicKey,
}

impl TransactionEncryptionKey {
    /// Constructs a key from the node's opaque identifier for it and its public key.
    ///
    /// The identifier changes when the key rotates, which is what lets a cached key be recognized
    /// as stale. It is treated as opaque bytes: the node derives it from the public key commitment
    /// but documents the encoding as an implementation detail.
    pub fn new(key_id: Vec<u8>, public_key: PublicKey) -> Self {
        Self { key_id, public_key }
    }

    /// Returns the node's opaque identifier for this key.
    pub fn key_id(&self) -> &[u8] {
        &self.key_id
    }

    /// Returns the public key.
    pub fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    /// Builds the sealing key used to encrypt transaction inputs against this key.
    pub fn sealing_key(&self) -> SealingKey {
        SealingKey::X25519XChaCha20Poly1305(self.public_key.clone())
    }
}

impl Serializable for TransactionEncryptionKey {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write_usize(self.key_id.len());
        target.write_bytes(&self.key_id);
        self.public_key.write_into(target);
    }
}

impl Deserializable for TransactionEncryptionKey {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let key_id_len = source.read_usize()?;
        let key_id = source.read_vec(key_id_len)?;
        let public_key = PublicKey::read_from(source)?;

        Ok(Self::new(key_id, public_key))
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
    pub rotation_block_num: u32,
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
    /// Validator attestations over [`attestation_commitment`], as `(validator key, signature)`.
    pub attestations: Vec<(ValidatorPublicKey, ValidatorSignature)>,
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

        let commitment = attestation_commitment(
            self.scheme,
            &self.key_id,
            genesis_commitment,
            &self.public_key,
            self.next_key.as_ref(),
        );

        let recognized = validator_keys.as_keys();
        let attested = self.attestations.iter().any(|(validator_key, signature)| {
            recognized.contains(validator_key) && validator_key.verify(commitment, signature)
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

        Ok(TransactionEncryptionKey::new(self.key_id, public_key))
    }
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
        payload.extend_from_slice(&next.rotation_block_num.to_le_bytes());
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
        .sealing_key()
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
        let key = TransactionEncryptionKey::new(b"key-id".to_vec(), secret_key.public_key());

        (key, UnsealingKey::X25519XChaCha20Poly1305(secret_key))
    }

    fn seal(key: &TransactionEncryptionKey, associated_data: &[u8]) -> Vec<u8> {
        key.sealing_key()
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

    // ATTESTATION VERIFICATION
    // --------------------------------------------------------------------------------------------

    use miden_protocol::block::ValidatorKeys;
    use miden_protocol::crypto::dsa::ecdsa_k256_keccak::SigningKey as ValidatorSigningKey;

    /// Builds a response attested by `signer`, the way a validator serves one.
    fn attested(
        key: &TransactionEncryptionKey,
        signer: &ValidatorSigningKey,
        genesis_commitment: Word,
    ) -> AttestedTransactionEncryptionKey {
        let public_key = key.public_key().to_bytes();
        let commitment = attestation_commitment(
            SUPPORTED_SCHEME,
            key.key_id(),
            genesis_commitment,
            &public_key,
            None,
        );

        AttestedTransactionEncryptionKey {
            scheme: SUPPORTED_SCHEME,
            key_id: key.key_id().to_vec(),
            public_key,
            attestations: vec![(signer.public_key(), signer.sign(commitment))],
            next_key: None,
        }
    }

    #[test]
    fn verify_accepts_an_attestation_from_a_recognized_validator() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let verified = attested(&key, &signer, Word::empty())
            .verify(Word::empty(), &validator_keys)
            .unwrap();

        assert_eq!(verified, key);
    }

    #[test]
    fn verify_rejects_a_validator_absent_from_the_committed_set() {
        let (key, _) = key_pair();
        let impostor = ValidatorSigningKey::with_rng(&mut rng());
        let committed = ValidatorSigningKey::with_rng(&mut ChaCha20Rng::seed_from_u64(7));
        let validator_keys = ValidatorKeys::new(vec![committed.public_key()]).unwrap();

        assert!(
            attested(&key, &impostor, Word::empty())
                .verify(Word::empty(), &validator_keys)
                .is_err()
        );
    }

    /// The whole point of the attestation: a substituted public key must not verify, even though
    /// the signature itself is genuine.
    #[test]
    fn verify_rejects_a_substituted_public_key() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let substitute = KeyExchangeKey::with_rng(&mut ChaCha20Rng::seed_from_u64(99));
        let mut response = attested(&key, &signer, Word::empty());
        response.public_key = substitute.public_key().to_bytes();

        assert!(response.verify(Word::empty(), &validator_keys).is_err());
    }

    /// The genesis commitment scopes an attestation to one chain, so the same signed response must
    /// not verify against a different network.
    #[test]
    fn verify_rejects_an_attestation_from_another_network() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let response = attested(&key, &signer, Word::empty());

        assert!(response.verify(Word::from([1u32, 2, 3, 4]), &validator_keys).is_err());
    }

    #[test]
    fn verify_rejects_an_unsupported_scheme() {
        let (key, _) = key_pair();
        let signer = ValidatorSigningKey::with_rng(&mut rng());
        let validator_keys = ValidatorKeys::new(vec![signer.public_key()]).unwrap();

        let mut response = attested(&key, &signer, Word::empty());
        response.scheme = SUPPORTED_SCHEME + 1;

        assert!(response.verify(Word::empty(), &validator_keys).is_err());
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
            rotation_block_num: 7,
        };
        let with_rotation =
            attestation_commitment(1, b"golden-key-id", genesis, b"golden-public-key", Some(&next));
        assert_eq!(
            with_rotation.to_hex(),
            "0xddfd7907b6a1ea6f294809ff0ed775f270b649ca15b21f88127c8335945e4752"
        );
    }
}

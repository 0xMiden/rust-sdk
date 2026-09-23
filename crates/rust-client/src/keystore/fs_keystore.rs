use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use miden_crypto::aead::xchacha::{EncryptedData, SecretKey as EncryptionKey};
use miden_crypto::utils::zeroize::Zeroizing;
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::account::auth::{
    AuthScheme,
    AuthSecretKey,
    PublicKey,
    PublicKeyCommitment,
    Signature,
};
use miden_tx::AuthenticationError;
use miden_tx::auth::{SigningInputs, TransactionAuthenticator};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};
use miden_tx::utils::sync::RwLock;
use serde::{Deserialize, Serialize};

use super::{KeyStoreError, Keystore};

// INDEX FILE
// ================================================================================================

const INDEX_FILE_NAME: &str = "key_index.json";
const INDEX_VERSION: u32 = 1;

/// The structure of the key index file that maps account IDs to public key commitments.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct KeyIndex {
    version: u32,
    /// Maps account ID (hex) to a set of public key commitment (hex).
    mappings: BTreeMap<String, BTreeSet<String>>,
}

impl KeyIndex {
    fn new() -> Self {
        Self {
            version: INDEX_VERSION,
            mappings: BTreeMap::new(),
        }
    }

    /// Adds a mapping from account ID to public key commitment.
    fn add_mapping(&mut self, account_id: &AccountId, pub_key_commitment: PublicKeyCommitment) {
        let account_id_hex = account_id.to_hex();
        let pub_key_hex = Word::from(pub_key_commitment).to_hex();

        self.mappings.entry(account_id_hex).or_default().insert(pub_key_hex);
    }

    /// Removes a mapping from an account ID to a public key commitment.
    ///
    /// Returns `true` if the mapping was present. An account entry that keeps no commitment is
    /// removed.
    fn remove_mapping(
        &mut self,
        account_id: &AccountId,
        pub_key_commitment: PublicKeyCommitment,
    ) -> bool {
        let account_id_hex = account_id.to_hex();
        let pub_key_hex = Word::from(pub_key_commitment).to_hex();

        let Some(commitments) = self.mappings.get_mut(&account_id_hex) else {
            return false;
        };

        let removed = commitments.remove(&pub_key_hex);
        if commitments.is_empty() {
            self.mappings.remove(&account_id_hex);
        }

        removed
    }

    /// Removes all mappings for a given public key commitment.
    fn remove_all_mappings_for_key(&mut self, pub_key_commitment: PublicKeyCommitment) {
        let pub_key_hex = Word::from(pub_key_commitment).to_hex();

        // Remove the key from all account mappings
        self.mappings.retain(|_, commitments| {
            commitments.remove(&pub_key_hex);
            !commitments.is_empty()
        });
    }

    /// Loads the index from disk, or creates a new one if it doesn't exist.
    fn read_from_file(keys_directory: &Path) -> Result<Self, KeyStoreError> {
        let index_path = keys_directory.join(INDEX_FILE_NAME);

        if !index_path.exists() {
            return Ok(Self::new());
        }

        let contents =
            fs::read_to_string(&index_path).map_err(keystore_error("error reading index file"))?;

        serde_json::from_str(&contents).map_err(|err| {
            KeyStoreError::DecodingError(format!("error parsing index file: {err:?}"))
        })
    }

    /// Saves the index to disk atomically (write to temp file, then rename).
    fn write_to_file(&self, keys_directory: &Path) -> Result<(), KeyStoreError> {
        let contents = serde_json::to_string_pretty(self).map_err(|err| {
            KeyStoreError::StorageError(format!("error serializing index: {err:?}"))
        })?;

        write_file_atomically(keys_directory, INDEX_FILE_NAME, contents.as_bytes())
    }

    /// Returns the account ID associated with a given public key commitment hex.
    ///
    /// Iterates over all mappings to find which account contains the commitment. Returns `None` if
    /// no account is found.
    ///
    /// A key can be associated with more than one account. This method returns only the first
    /// account in iteration order. Use [`KeyIndex::get_account_ids`] to get every account.
    fn get_account_id(&self, pub_key_commitment: PublicKeyCommitment) -> Option<AccountId> {
        let pub_key_hex = Word::from(pub_key_commitment).to_hex();

        for (account_id_hex, commitments) in &self.mappings {
            if commitments.contains(&pub_key_hex) {
                return AccountId::from_hex(account_id_hex).ok();
            }
        }

        None
    }

    /// Returns all account IDs associated with a public key commitment.
    fn get_account_ids(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<BTreeSet<AccountId>, KeyStoreError> {
        let pub_key_hex = Word::from(pub_key_commitment).to_hex();

        self.mappings
            .iter()
            .filter(|(_, commitments)| commitments.contains(&pub_key_hex))
            .map(|(account_id_hex, _)| {
                AccountId::from_hex(account_id_hex).map_err(|err| {
                    KeyStoreError::DecodingError(format!(
                        "error parsing account ID in key index: {err:?}"
                    ))
                })
            })
            .collect()
    }

    /// Gets all public key commitments for an account ID.
    ///
    /// Returns an empty set if the index holds no mapping for the account. An account can hold keys
    /// that this keystore does not have, so an absent mapping is a valid state.
    fn get_commitments(&self, account_id: &AccountId) -> BTreeSet<PublicKeyCommitment> {
        let account_id_hex = account_id.to_hex();

        self.mappings
            .get(&account_id_hex)
            .map(|commitments| {
                commitments
                    .iter()
                    .filter_map(|hex| {
                        Word::try_from(hex.as_str()).ok().map(PublicKeyCommitment::from)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

// ENCRYPTION METADATA FILE
// ================================================================================================

/// Name of the file that marks an encrypted keystore and holds its key derivation parameters.
const ENCRYPTION_FILE_NAME: &str = "encryption.bin";
const ENCRYPTION_VERSION: u32 = 1;
const SALT_SIZE_BYTES: usize = 16;
const ENCRYPTION_KEY_SIZE_BYTES: usize = 32;
/// Plaintext of the check value. A key that decrypts it to this value is the correct key.
const CHECK_VALUE: &[u8] = b"miden-client keystore";

/// Parameters that derive the encryption key of an encrypted keystore from its password.
///
/// The key derivation function is Argon2id and the cipher is XChaCha20-Poly1305. The parameters are
/// stored with the salt so that a keystore stays readable when the defaults change.
struct EncryptionMetadata {
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
    salt: [u8; SALT_SIZE_BYTES],
    /// [`CHECK_VALUE`] encrypted with the derived key. Decrypting it verifies the password.
    check: EncryptedData,
}

impl EncryptionMetadata {
    /// Creates metadata with a random salt and derives the key for `password`.
    fn new(password: &[u8]) -> Result<(Self, EncryptionKey), KeyStoreError> {
        let salt: [u8; SALT_SIZE_BYTES] = rand::random();
        let key = derive_key(
            password,
            &salt,
            argon2::Params::DEFAULT_M_COST,
            argon2::Params::DEFAULT_T_COST,
            argon2::Params::DEFAULT_P_COST,
        )?;
        let check = key.encrypt_bytes(CHECK_VALUE).map_err(encryption_error)?;

        let metadata = Self {
            m_cost: argon2::Params::DEFAULT_M_COST,
            t_cost: argon2::Params::DEFAULT_T_COST,
            p_cost: argon2::Params::DEFAULT_P_COST,
            salt,
            check,
        };
        Ok((metadata, key))
    }

    /// Derives the key for `password` and verifies it against the check value.
    fn unlock(&self, password: &[u8]) -> Result<EncryptionKey, KeyStoreError> {
        let key = derive_key(password, &self.salt, self.m_cost, self.t_cost, self.p_cost)?;
        let decrypted =
            key.decrypt_bytes(&self.check).map_err(|_| KeyStoreError::InvalidPassword)?;
        if decrypted != CHECK_VALUE {
            return Err(KeyStoreError::InvalidPassword);
        }
        Ok(key)
    }

    fn read_from_file(keys_directory: &Path) -> Result<Self, KeyStoreError> {
        let bytes = fs::read(keys_directory.join(ENCRYPTION_FILE_NAME))
            .map_err(keystore_error("error reading encryption metadata file"))?;
        Self::read_from_bytes(&bytes).map_err(|err| {
            KeyStoreError::DecodingError(format!("error parsing encryption metadata file: {err}"))
        })
    }

    fn write_to_file(&self, keys_directory: &Path) -> Result<(), KeyStoreError> {
        write_file_atomically(keys_directory, ENCRYPTION_FILE_NAME, &self.to_bytes())
    }
}

impl Serializable for EncryptionMetadata {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write_u32(ENCRYPTION_VERSION);
        target.write_u32(self.m_cost);
        target.write_u32(self.t_cost);
        target.write_u32(self.p_cost);
        target.write_bytes(&self.salt);
        self.check.write_into(target);
    }
}

impl Deserializable for EncryptionMetadata {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let version = source.read_u32()?;
        if version != ENCRYPTION_VERSION {
            return Err(DeserializationError::InvalidValue(format!(
                "unsupported keystore encryption version {version}"
            )));
        }
        Ok(Self {
            m_cost: source.read_u32()?,
            t_cost: source.read_u32()?,
            p_cost: source.read_u32()?,
            salt: source.read_array()?,
            check: EncryptedData::read_from(source)?,
        })
    }
}

/// Derives the encryption key from `password` with Argon2id.
fn derive_key(
    password: &[u8],
    salt: &[u8; SALT_SIZE_BYTES],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> Result<EncryptionKey, KeyStoreError> {
    let params = argon2::Params::new(m_cost, t_cost, p_cost, Some(ENCRYPTION_KEY_SIZE_BYTES))
        .map_err(encryption_error)?;
    let argon2 = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);

    let mut key_bytes = Zeroizing::new([0u8; ENCRYPTION_KEY_SIZE_BYTES]);
    argon2
        .hash_password_into(password, salt, key_bytes.as_mut())
        .map_err(encryption_error)?;

    EncryptionKey::read_from_bytes(key_bytes.as_ref()).map_err(encryption_error)
}

// FILESYSTEM KEYSTORE
// ================================================================================================

/// A filesystem-based keystore that stores keys in separate files and provides transaction
/// authentication functionality. The public key is hashed and the result is used as the filename
/// and the contents of the file are the serialized public and secret key.
///
/// Account-to-key mappings are stored in a separate JSON index file.
///
/// A keystore created with [`FilesystemKeyStore::new`] encrypts each key file with a key derived
/// from a password, and holds that key in memory while it is open. A keystore created with
/// [`FilesystemKeyStore::new_plaintext`] writes the secret keys in plaintext. Use it only for
/// development.
#[derive(Debug)]
pub struct FilesystemKeyStore {
    /// The directory where the keys are stored and read from.
    pub keys_directory: PathBuf,
    /// The in-memory index of account-to-key mappings.
    index: RwLock<KeyIndex>,
    /// The key that encrypts the key files. `None` for a plaintext keystore.
    encryption_key: Option<Arc<EncryptionKey>>,
}

/// Information about a secret key in a [`FilesystemKeyStore`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredKeyInfo {
    pub commitment: PublicKeyCommitment,
    pub scheme: AuthScheme,
    pub account_ids: BTreeSet<AccountId>,
}

impl Clone for FilesystemKeyStore {
    fn clone(&self) -> Self {
        let index = self.index.read().clone();
        Self {
            keys_directory: self.keys_directory.clone(),
            index: RwLock::new(index),
            encryption_key: self.encryption_key.clone(),
        }
    }
}

impl FilesystemKeyStore {
    /// Creates a [`FilesystemKeyStore`] on a specific directory that encrypts the key files with a
    /// key derived from `password`.
    ///
    /// A directory without a keystore gets a new random salt. The password of an existing encrypted
    /// keystore is verified before the keystore opens. The derived key stays in memory while the
    /// keystore is alive.
    ///
    /// # Errors
    ///
    /// Returns [`KeyStoreError::InvalidPassword`] if the password does not match the existing
    /// keystore, and an error if the directory holds plaintext keys. Plaintext keys are encrypted
    /// with [`FilesystemKeyStore::encrypt_plaintext_keystore`].
    pub fn new(keys_directory: PathBuf, password: &[u8]) -> Result<Self, KeyStoreError> {
        Self::create_keys_directory(&keys_directory)?;

        let key = if Self::is_encrypted_directory(&keys_directory) {
            EncryptionMetadata::read_from_file(&keys_directory)?.unlock(password)?
        } else {
            if Self::holds_plaintext_keys(&keys_directory)? {
                return Err(KeyStoreError::StorageError(format!(
                    "keystore at {} holds plaintext keys; encrypt them before opening it with a \
                     password",
                    keys_directory.display()
                )));
            }
            let (metadata, key) = EncryptionMetadata::new(password)?;
            metadata.write_to_file(&keys_directory)?;
            key
        };

        Self::open(keys_directory, Some(Arc::new(key)))
    }

    /// Creates a [`FilesystemKeyStore`] on a specific directory.
    ///
    /// # Security
    ///
    /// The secret keys are written to disk in plaintext. Anyone who can read the directory can
    /// spend from the accounts that these keys control. This keystore is only recommended for
    /// development. Use [`FilesystemKeyStore::new`] for keys that control real funds.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory holds an encrypted keystore.
    pub fn new_plaintext(keys_directory: PathBuf) -> Result<Self, KeyStoreError> {
        Self::create_keys_directory(&keys_directory)?;

        if Self::is_encrypted_directory(&keys_directory) {
            return Err(KeyStoreError::StorageError(format!(
                "keystore at {} is encrypted and requires a password",
                keys_directory.display()
            )));
        }

        Self::open(keys_directory, None)
    }

    /// Encrypts every key file of a plaintext keystore with a key derived from `password`.
    ///
    /// Returns the encrypted keystore and the commitments of the key files that do not hold a
    /// readable key. These files are left as they are, so the caller must report them. The account
    /// associations are kept. Each key file is replaced atomically, so a key file is never
    /// partially written.
    ///
    /// If this operation stops before it completes, the directory holds both encrypted and
    /// plaintext key files. Call this function again with the same password to encrypt the
    /// remaining plaintext key files. A call on a keystore that is fully encrypted changes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`KeyStoreError::InvalidPassword`] if the directory already holds an encrypted
    /// keystore with a different password, and an error if a key file cannot be read.
    pub fn encrypt_plaintext_keystore(
        keys_directory: PathBuf,
        password: &[u8],
    ) -> Result<(Self, Vec<PublicKeyCommitment>), KeyStoreError> {
        Self::create_keys_directory(&keys_directory)?;

        // The metadata file is written before the key files are encrypted. An interrupted operation
        // thus leaves the metadata file that a new call needs to continue.
        let key = if Self::is_encrypted_directory(&keys_directory) {
            EncryptionMetadata::read_from_file(&keys_directory)?.unlock(password)?
        } else {
            let (metadata, key) = EncryptionMetadata::new(password)?;
            metadata.write_to_file(&keys_directory)?;
            key
        };
        let encrypted = Self::open(keys_directory, Some(Arc::new(key)))?;

        let mut unreadable = Vec::new();
        for commitment in key_file_commitments(&encrypted.keys_directory)? {
            let bytes = fs::read(key_file_path(&encrypted.keys_directory, commitment))
                .map_err(keystore_error("error reading secret key file"))?;
            if encrypted.decode_key(&bytes, commitment).is_ok() {
                continue;
            }
            match AuthSecretKey::read_from_bytes(&bytes) {
                Ok(key) => encrypted.store_key(&key)?,
                Err(_) => unreadable.push(commitment),
            }
        }

        Ok((encrypted, unreadable))
    }

    /// Returns `true` if the key files are encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.encryption_key.is_some()
    }

    /// Returns `true` if `keys_directory` holds an encrypted keystore.
    pub fn is_encrypted_directory(keys_directory: &Path) -> bool {
        keys_directory.join(ENCRYPTION_FILE_NAME).exists()
    }

    /// Returns `true` if `keys_directory` is not an encrypted keystore and holds key files.
    ///
    /// [`FilesystemKeyStore::new`] refuses such a directory. A directory that is missing or holds
    /// no key files becomes a new encrypted keystore.
    ///
    /// # Errors
    ///
    /// Returns an error if the directory cannot be read.
    pub fn holds_plaintext_keys(keys_directory: &Path) -> Result<bool, KeyStoreError> {
        if !keys_directory.exists() || Self::is_encrypted_directory(keys_directory) {
            return Ok(false);
        }
        Ok(!key_file_commitments(keys_directory)?.is_empty())
    }

    fn create_keys_directory(keys_directory: &Path) -> Result<(), KeyStoreError> {
        if !keys_directory.exists() {
            fs::create_dir_all(keys_directory)
                .map_err(keystore_error("error creating keys directory"))?;
        }
        Ok(())
    }

    fn open(
        keys_directory: PathBuf,
        encryption_key: Option<Arc<EncryptionKey>>,
    ) -> Result<Self, KeyStoreError> {
        let index = KeyIndex::read_from_file(&keys_directory)?;

        Ok(FilesystemKeyStore {
            keys_directory,
            index: RwLock::new(index),
            encryption_key,
        })
    }

    /// Stores a secret key without associating it with an account.
    pub fn store_key(&self, key: &AuthSecretKey) -> Result<(), KeyStoreError> {
        let pub_key_commitment = key.public_key().to_commitment();
        let contents = self.encode_key(key, pub_key_commitment)?;
        write_secret_key_file(&self.keys_directory, pub_key_commitment, &contents)
    }

    /// Returns information about all secret keys in the keystore.
    pub fn list_keys(&self) -> Result<Vec<StoredKeyInfo>, KeyStoreError> {
        let index = self.index.read().clone();
        let mut keys = Vec::new();

        for commitment in key_file_commitments(&self.keys_directory)? {
            // A file that does not hold a readable key must not hide the keys that are readable. An
            // interrupted write leaves such a file behind, so `list_keys` skips it and reports the
            // keys it can read.
            let Ok(Some(key)) = self.get_key_sync(commitment) else {
                continue;
            };
            if key.public_key().to_commitment() != commitment {
                continue;
            }

            keys.push(StoredKeyInfo {
                commitment,
                scheme: key.auth_scheme(),
                account_ids: index.get_account_ids(commitment).unwrap_or_default(),
            });
        }

        keys.sort_by_key(|key| Word::from(key.commitment).to_hex());
        Ok(keys)
    }

    /// Returns all account IDs associated with a public key commitment.
    pub fn account_ids_for_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<BTreeSet<AccountId>, KeyStoreError> {
        self.index.read().get_account_ids(pub_key_commitment)
    }

    /// Associates a stored key with an account.
    pub fn associate_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
        account_id: AccountId,
    ) -> Result<(), KeyStoreError> {
        let key = self.get_key_sync(pub_key_commitment)?.ok_or_else(|| {
            KeyStoreError::StorageError(format!(
                "secret key not found for commitment {}",
                Word::from(pub_key_commitment).to_hex()
            ))
        })?;
        if key.public_key().to_commitment() != pub_key_commitment {
            return Err(KeyStoreError::DecodingError(format!(
                "key file content does not match commitment {}",
                Word::from(pub_key_commitment).to_hex()
            )));
        }

        self.index.write().add_mapping(&account_id, pub_key_commitment);
        self.save_index()
    }

    /// Removes the association between a stored key and an account.
    ///
    /// Returns `true` if the association was present. The index is written only when it changes.
    pub fn disassociate_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
        account_id: AccountId,
    ) -> Result<bool, KeyStoreError> {
        let removed = self.index.write().remove_mapping(&account_id, pub_key_commitment);
        if !removed {
            return Ok(false);
        }

        self.save_index()?;
        Ok(true)
    }

    /// Retrieves a secret key from the keystore given the commitment of a public key.
    pub fn get_key_sync(
        &self,
        pub_key: PublicKeyCommitment,
    ) -> Result<Option<AuthSecretKey>, KeyStoreError> {
        let file_path = key_file_path(&self.keys_directory, pub_key);
        match fs::read(&file_path) {
            Ok(bytes) => Ok(Some(self.decode_key(&bytes, pub_key)?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(keystore_error("error reading secret key file")(e)),
        }
    }

    /// Returns the contents of the key file for `key`.
    ///
    /// An encrypted keystore binds the ciphertext to the commitment, so a key file that is renamed
    /// to another commitment does not decrypt.
    fn encode_key(
        &self,
        key: &AuthSecretKey,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<Vec<u8>, KeyStoreError> {
        let Some(encryption_key) = &self.encryption_key else {
            return Ok(key.to_bytes());
        };

        let plaintext = Zeroizing::new(key.to_bytes());
        let encrypted = encryption_key
            .encrypt_bytes_with_associated_data(
                &plaintext,
                &Word::from(pub_key_commitment).to_bytes(),
            )
            .map_err(encryption_error)?;
        Ok(encrypted.to_bytes())
    }

    /// Reads a key from the contents of its key file.
    fn decode_key(
        &self,
        bytes: &[u8],
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<AuthSecretKey, KeyStoreError> {
        let decoding_error = |err: DeserializationError| {
            KeyStoreError::DecodingError(format!("error reading secret key from file: {err:?}"))
        };

        let Some(encryption_key) = &self.encryption_key else {
            return AuthSecretKey::read_from_bytes(bytes).map_err(decoding_error);
        };

        let encrypted = EncryptedData::read_from_bytes(bytes).map_err(decoding_error)?;
        let plaintext = encryption_key
            .decrypt_bytes_with_associated_data(
                &encrypted,
                &Word::from(pub_key_commitment).to_bytes(),
            )
            .map(Zeroizing::new)
            .map_err(|err| {
                KeyStoreError::DecodingError(format!("error decrypting secret key file: {err}"))
            })?;
        AuthSecretKey::read_from_bytes(&plaintext).map_err(decoding_error)
    }

    /// Saves the index to disk.
    fn save_index(&self) -> Result<(), KeyStoreError> {
        let index = self.index.read();
        index.write_to_file(&self.keys_directory)
    }
}

impl TransactionAuthenticator for FilesystemKeyStore {
    /// Gets a signature over a message, given a public key.
    ///
    /// The public key should correspond to one of the keys tracked by the keystore.
    ///
    /// # Errors
    /// If the public key isn't found in the store, [`AuthenticationError::UnknownPublicKey`] is
    /// returned.
    // The trait declares this method as async; this implementation signs from local state and has
    // nothing to await.
    #[allow(clippy::unused_async_trait_impl, reason = "the trait signature is async")]
    async fn get_signature(
        &self,
        pub_key: PublicKeyCommitment,
        signing_info: &SigningInputs,
    ) -> Result<Signature, AuthenticationError> {
        let message = signing_info.to_commitment();

        let secret_key = self
            .get_key_sync(pub_key)
            .map_err(|err| {
                AuthenticationError::other_with_source("failed to load secret key", err)
            })?
            .ok_or(AuthenticationError::UnknownPublicKey(pub_key))?;

        Ok(secret_key.sign(message))
    }

    /// Retrieves a public key for a specific public key commitment.
    async fn get_public_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Option<Arc<PublicKey>> {
        self.get_key(pub_key_commitment)
            .await
            .ok()
            .flatten()
            .map(|key| Arc::new(key.public_key()))
    }
}

#[async_trait::async_trait]
impl Keystore for FilesystemKeyStore {
    async fn add_key(
        &self,
        key: &AuthSecretKey,
        account_id: AccountId,
    ) -> Result<(), KeyStoreError> {
        let pub_key_commitment = key.public_key().to_commitment();

        self.store_key(key)?;

        {
            let mut index = self.index.write();
            index.add_mapping(&account_id, pub_key_commitment);
        }

        // Persist the index
        self.save_index()?;

        Ok(())
    }

    async fn remove_key(&self, pub_key: PublicKeyCommitment) -> Result<(), KeyStoreError> {
        // Remove from index first
        {
            let mut index = self.index.write();
            index.remove_all_mappings_for_key(pub_key);
        }

        // Persist the index
        self.save_index()?;

        // Remove the key file
        let file_path = key_file_path(&self.keys_directory, pub_key);
        match fs::remove_file(file_path) {
            Ok(()) => {},
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(keystore_error("error removing secret key file")(e)),
        }

        Ok(())
    }

    async fn get_key(
        &self,
        pub_key: PublicKeyCommitment,
    ) -> Result<Option<AuthSecretKey>, KeyStoreError> {
        self.get_key_sync(pub_key)
    }

    async fn get_account_id_by_key_commitment(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<Option<AccountId>, KeyStoreError> {
        let index = self.index.read();
        Ok(index.get_account_id(pub_key_commitment))
    }

    async fn get_account_key_commitments(
        &self,
        account_id: &AccountId,
    ) -> Result<BTreeSet<PublicKeyCommitment>, KeyStoreError> {
        let index = self.index.read();
        Ok(index.get_commitments(account_id))
    }
}

// HELPERS
// ================================================================================================

/// Returns the file path that belongs to the public key commitment
fn key_file_path(keys_directory: &Path, pub_key_commitment: PublicKeyCommitment) -> PathBuf {
    let filename = Word::from(pub_key_commitment).to_hex();
    keys_directory.join(filename)
}

/// Returns the commitments of the files in the keys directory that are named after a commitment.
///
/// The index and encryption metadata files are not included. A file in the list does not have to
/// hold a readable key.
fn key_file_commitments(keys_directory: &Path) -> Result<Vec<PublicKeyCommitment>, KeyStoreError> {
    let mut commitments = Vec::new();

    for entry in
        fs::read_dir(keys_directory).map_err(keystore_error("error reading keys directory"))?
    {
        let entry = entry.map_err(keystore_error("error reading keys directory entry"))?;
        if !entry
            .file_type()
            .map_err(keystore_error("error reading key file type"))?
            .is_file()
        {
            continue;
        }

        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            continue;
        };
        if let Ok(commitment) = Word::try_from(file_name).map(PublicKeyCommitment::from) {
            commitments.push(commitment);
        }
    }

    Ok(commitments)
}

/// Writes `contents` to `file_name` in `directory` atomically (write to temp file, then rename).
fn write_file_atomically(
    directory: &Path,
    file_name: &str,
    contents: &[u8],
) -> Result<(), KeyStoreError> {
    let file_path = directory.join(file_name);

    // Create the temp file in the same directory as the target so the subsequent atomic rename
    // stays on the same filesystem.
    let mut temp_file = tempfile::NamedTempFile::new_in(directory)
        .map_err(keystore_error("error creating temp file"))?;
    temp_file
        .write_all(contents)
        .map_err(keystore_error("error writing temp file"))?;
    temp_file
        .as_file()
        .sync_all()
        .map_err(keystore_error("error syncing temp file"))?;

    // Atomically replace the target file.
    temp_file.persist(&file_path).map_err(|err| {
        keystore_error(&format!("error renaming temp file to {file_name}"))(err.error)
    })?;

    Ok(())
}

/// Writes the contents of a key file atomically (write to temp file, then rename).
///
/// On Unix, the temp file is created with restrictive permissions (0600), and the key file keeps
/// them after the rename.
// TODO: on Windows, set restrictive ACLs to limit access to the current user.
fn write_secret_key_file(
    keys_directory: &Path,
    pub_key_commitment: PublicKeyCommitment,
    contents: &[u8],
) -> Result<(), KeyStoreError> {
    let file_name = Word::from(pub_key_commitment).to_hex();
    write_file_atomically(keys_directory, &file_name, contents)
}

fn keystore_error(context: &str) -> impl FnOnce(std::io::Error) -> KeyStoreError {
    let context = String::from(context);
    move |err| KeyStoreError::StorageError(format!("{context}: {err:?}"))
}

fn encryption_error(err: impl core::fmt::Display) -> KeyStoreError {
    KeyStoreError::EncryptionError(format!("{err}"))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::account::auth::AuthSecretKey;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
    };

    use super::*;

    const PASSWORD: &[u8] = b"correct horse battery staple";

    /// Creates a keystore on a temporary directory. The directory is removed when the returned
    /// guard is dropped, so the guard must stay alive for the whole test.
    fn test_keystore() -> (FilesystemKeyStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().expect("should create a temporary directory");
        let keystore = FilesystemKeyStore::new_plaintext(dir.path().to_path_buf())
            .expect("should create a keystore on an existing directory");

        (keystore, dir)
    }

    fn test_account_id() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE)
            .expect("test account ID should be well formed")
    }

    /// Returns a commitment that no generated key produces, so the keystore never holds a key for
    /// it.
    fn unused_commitment() -> Word {
        Word::try_from("0x0000000000000000000000000000000000000000000000000000000000000001")
            .expect("the test commitment is a valid word")
    }

    fn other_account_id() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE)
            .expect("test account ID should be well formed")
    }

    #[test]
    fn standalone_key_is_listed_without_an_account() {
        let (keystore, _dir) = test_keystore();
        let key = AuthSecretKey::new_ecdsa_k256_keccak();
        let commitment = key.public_key().to_commitment();

        keystore.store_key(&key).unwrap();

        let stored_keys = keystore.list_keys().unwrap();
        assert_eq!(stored_keys.len(), 1);
        assert_eq!(stored_keys[0].commitment, commitment);
        assert_eq!(stored_keys[0].scheme, key.auth_scheme());
        assert!(stored_keys[0].account_ids.is_empty());
        assert!(keystore.account_ids_for_key(commitment).unwrap().is_empty());
    }

    #[tokio::test]
    async fn key_commitments_of_untracked_account_are_empty() {
        let (keystore, _dir) = test_keystore();

        let commitments = keystore
            .get_account_key_commitments(&test_account_id())
            .await
            .expect("an account without keys is not an error");
        assert!(commitments.is_empty());

        let keys = keystore
            .get_keys_for_account(&test_account_id())
            .await
            .expect("an account without keys is not an error");
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn key_commitments_are_scoped_to_their_account() {
        let (keystore, _dir) = test_keystore();
        let key = AuthSecretKey::new_falcon512_poseidon2();
        let commitment = key.public_key().to_commitment();

        keystore.add_key(&key, test_account_id()).await.unwrap();

        let commitments = keystore.get_account_key_commitments(&test_account_id()).await.unwrap();
        assert_eq!(commitments.len(), 1);
        assert!(commitments.contains(&commitment));

        let account_ids = keystore.account_ids_for_key(commitment).unwrap();
        assert_eq!(account_ids, BTreeSet::from([test_account_id()]));

        let commitments = keystore.get_account_key_commitments(&other_account_id()).await.unwrap();
        assert!(commitments.is_empty());

        keystore.disassociate_key(commitment, test_account_id()).unwrap();
        assert!(keystore.account_ids_for_key(commitment).unwrap().is_empty());
        assert!(keystore.get_key_sync(commitment).unwrap().is_some());
    }

    #[tokio::test]
    async fn key_commitments_are_empty_after_the_last_key_is_removed() {
        let (keystore, _dir) = test_keystore();
        let key = AuthSecretKey::new_falcon512_poseidon2();

        keystore.add_key(&key, test_account_id()).await.unwrap();
        keystore.remove_key(key.public_key().to_commitment()).await.unwrap();

        let commitments = keystore
            .get_account_key_commitments(&test_account_id())
            .await
            .expect("removing the last key of an account is not an error");
        assert!(commitments.is_empty());
    }

    /// A key can back more than one account. Removing one association must keep the others, and a
    /// removal that changes nothing must say so.
    #[tokio::test]
    async fn disassociating_a_key_affects_only_the_named_account() {
        let (keystore, _dir) = test_keystore();
        let shared_key = AuthSecretKey::new_falcon512_poseidon2();
        let shared_commitment = shared_key.public_key().to_commitment();

        keystore.add_key(&shared_key, test_account_id()).await.unwrap();
        keystore.add_key(&shared_key, other_account_id()).await.unwrap();

        assert!(keystore.disassociate_key(shared_commitment, test_account_id()).unwrap());
        assert_eq!(
            keystore.account_ids_for_key(shared_commitment).unwrap(),
            BTreeSet::from([other_account_id()]),
            "the key must stay associated with the account that was not named"
        );

        assert!(
            !keystore.disassociate_key(shared_commitment, test_account_id()).unwrap(),
            "the association is already gone"
        );
        assert!(
            !keystore
                .disassociate_key(unused_commitment().into(), other_account_id())
                .unwrap(),
            "no key is stored for this commitment"
        );
        assert_eq!(
            keystore.account_ids_for_key(shared_commitment).unwrap(),
            BTreeSet::from([other_account_id()]),
            "a call that changes nothing must not drop an existing association"
        );
    }

    /// An interrupted write leaves a file that holds no readable key. The keys that are readable
    /// must still be listed.
    #[test]
    fn unreadable_key_file_is_skipped_by_the_listing() {
        let (keystore, dir) = test_keystore();
        let key = AuthSecretKey::new_falcon512_poseidon2();
        let commitment = key.public_key().to_commitment();

        keystore.store_key(&key).unwrap();

        // A truncated key file under a name that is a valid commitment.
        fs::write(dir.path().join(unused_commitment().to_hex()), [1, 2, 3]).unwrap();
        // A file whose name is not a commitment at all.
        fs::write(dir.path().join(".DS_Store"), []).unwrap();

        let listed = keystore.list_keys().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].commitment, commitment);
    }

    // ENCRYPTION
    // --------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn encrypted_keystore_reopens_with_the_same_password_only() {
        let dir = tempfile::tempdir().unwrap();
        let key = AuthSecretKey::new_falcon512_poseidon2();
        let commitment = key.public_key().to_commitment();

        let keystore = FilesystemKeyStore::new(dir.path().to_path_buf(), PASSWORD).unwrap();
        assert!(keystore.is_encrypted());
        keystore.add_key(&key, test_account_id()).await.unwrap();

        let file = fs::read(key_file_path(dir.path(), commitment)).unwrap();
        assert_ne!(file, key.to_bytes(), "the key file must not hold the plaintext key");
        assert_eq!(keystore.get_key_sync(commitment).unwrap().unwrap().to_bytes(), key.to_bytes());
        assert_eq!(keystore.list_keys().unwrap().len(), 1);

        let reopened = FilesystemKeyStore::new(dir.path().to_path_buf(), PASSWORD).unwrap();
        assert_eq!(reopened.get_key_sync(commitment).unwrap().unwrap().to_bytes(), key.to_bytes());
        assert_eq!(
            reopened.account_ids_for_key(commitment).unwrap(),
            BTreeSet::from([test_account_id()])
        );

        let wrong_password = FilesystemKeyStore::new(dir.path().to_path_buf(), b"wrong");
        assert!(matches!(wrong_password, Err(KeyStoreError::InvalidPassword)));

        let plaintext = FilesystemKeyStore::new_plaintext(dir.path().to_path_buf());
        assert!(matches!(plaintext, Err(KeyStoreError::StorageError(_))));
    }

    /// The ciphertext is bound to the commitment in the file name, so a key file that is copied
    /// under another commitment does not decrypt.
    #[test]
    fn encrypted_key_file_does_not_decrypt_under_another_commitment() {
        let dir = tempfile::tempdir().unwrap();
        let key = AuthSecretKey::new_ecdsa_k256_keccak();
        let commitment = key.public_key().to_commitment();

        let keystore = FilesystemKeyStore::new(dir.path().to_path_buf(), PASSWORD).unwrap();
        keystore.store_key(&key).unwrap();
        fs::copy(
            key_file_path(dir.path(), commitment),
            dir.path().join(unused_commitment().to_hex()),
        )
        .unwrap();

        let renamed = keystore.get_key_sync(unused_commitment().into());
        assert!(matches!(renamed, Err(KeyStoreError::DecodingError(_))));
        assert_eq!(keystore.list_keys().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn plaintext_keystore_is_encrypted_in_place() {
        let (plaintext, dir) = test_keystore();
        let associated_key = AuthSecretKey::new_falcon512_poseidon2();
        let associated_commitment = associated_key.public_key().to_commitment();
        let standalone_key = AuthSecretKey::new_ecdsa_k256_keccak();
        let standalone_commitment = standalone_key.public_key().to_commitment();

        plaintext.add_key(&associated_key, test_account_id()).await.unwrap();
        plaintext.store_key(&standalone_key).unwrap();
        fs::write(dir.path().join(unused_commitment().to_hex()), [1, 2, 3]).unwrap();
        drop(plaintext);

        let opened_with_password = FilesystemKeyStore::new(dir.path().to_path_buf(), PASSWORD);
        assert!(
            matches!(opened_with_password, Err(KeyStoreError::StorageError(_))),
            "plaintext keys must not be silently mixed with encrypted keys"
        );

        let (encrypted, unreadable) =
            FilesystemKeyStore::encrypt_plaintext_keystore(dir.path().to_path_buf(), PASSWORD)
                .unwrap();
        assert!(encrypted.is_encrypted());
        assert_eq!(unreadable, vec![PublicKeyCommitment::from(unused_commitment())]);
        assert_ne!(
            fs::read(key_file_path(dir.path(), associated_commitment)).unwrap(),
            associated_key.to_bytes()
        );
        assert_eq!(
            encrypted.get_key_sync(associated_commitment).unwrap().unwrap().to_bytes(),
            associated_key.to_bytes()
        );
        assert_eq!(
            encrypted.get_key_sync(standalone_commitment).unwrap().unwrap().to_bytes(),
            standalone_key.to_bytes()
        );
        assert_eq!(
            encrypted.account_ids_for_key(associated_commitment).unwrap(),
            BTreeSet::from([test_account_id()])
        );
        assert_eq!(encrypted.list_keys().unwrap().len(), 2);

        let (again, _) =
            FilesystemKeyStore::encrypt_plaintext_keystore(dir.path().to_path_buf(), PASSWORD)
                .unwrap();
        assert_eq!(again.list_keys().unwrap().len(), 2);

        let wrong_password =
            FilesystemKeyStore::encrypt_plaintext_keystore(dir.path().to_path_buf(), b"wrong");
        assert!(matches!(wrong_password, Err(KeyStoreError::InvalidPassword)));
    }

    /// A directory with files that are not key files is a new keystore, not a plaintext keystore.
    #[test]
    fn directory_without_key_files_opens_as_new_encrypted_keystore() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(".DS_Store"), [1, 2, 3]).unwrap();
        assert!(!FilesystemKeyStore::holds_plaintext_keys(dir.path()).unwrap());

        let keystore = FilesystemKeyStore::new(dir.path().to_path_buf(), PASSWORD).unwrap();
        assert!(keystore.is_encrypted());
        assert!(!FilesystemKeyStore::holds_plaintext_keys(dir.path()).unwrap());

        let (plaintext, plaintext_dir) = test_keystore();
        plaintext.store_key(&AuthSecretKey::new_falcon512_poseidon2()).unwrap();
        assert!(FilesystemKeyStore::holds_plaintext_keys(plaintext_dir.path()).unwrap());
    }

    /// An interrupted encryption leaves plaintext key files next to encrypted ones. A new call with
    /// the same password encrypts the remaining plaintext key files.
    #[test]
    fn interrupted_encryption_completes_on_a_new_call() {
        let dir = tempfile::tempdir().unwrap();
        let encrypted_key = AuthSecretKey::new_falcon512_poseidon2();
        let encrypted_commitment = encrypted_key.public_key().to_commitment();
        let plaintext_key = AuthSecretKey::new_ecdsa_k256_keccak();
        let plaintext_commitment = plaintext_key.public_key().to_commitment();

        let keystore = FilesystemKeyStore::new(dir.path().to_path_buf(), PASSWORD).unwrap();
        keystore.store_key(&encrypted_key).unwrap();
        fs::write(key_file_path(dir.path(), plaintext_commitment), plaintext_key.to_bytes())
            .unwrap();
        drop(keystore);

        let (encrypted, unreadable) =
            FilesystemKeyStore::encrypt_plaintext_keystore(dir.path().to_path_buf(), PASSWORD)
                .unwrap();
        assert!(unreadable.is_empty());
        assert_ne!(
            fs::read(key_file_path(dir.path(), plaintext_commitment)).unwrap(),
            plaintext_key.to_bytes()
        );
        assert_eq!(
            encrypted.get_key_sync(plaintext_commitment).unwrap().unwrap().to_bytes(),
            plaintext_key.to_bytes()
        );
        assert_eq!(
            encrypted.get_key_sync(encrypted_commitment).unwrap().unwrap().to_bytes(),
            encrypted_key.to_bytes()
        );
        assert_eq!(encrypted.list_keys().unwrap().len(), 2);
    }
}

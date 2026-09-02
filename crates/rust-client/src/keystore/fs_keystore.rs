use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::account::auth::{AuthSecretKey, PublicKey, PublicKeyCommitment, Signature};
use miden_tx::AuthenticationError;
use miden_tx::auth::{SigningInputs, TransactionAuthenticator};
use miden_tx::utils::serde::{Deserializable, Serializable};
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
        let index_path = keys_directory.join(INDEX_FILE_NAME);

        let contents = serde_json::to_string_pretty(self).map_err(|err| {
            KeyStoreError::StorageError(format!("error serializing index: {err:?}"))
        })?;

        // Create the temp file in the same directory as the index so the subsequent atomic
        // rename stays on the same filesystem.
        let mut temp_file = tempfile::NamedTempFile::new_in(keys_directory)
            .map_err(keystore_error("error creating temp index file"))?;
        temp_file
            .write_all(contents.as_bytes())
            .map_err(keystore_error("error writing temp index file"))?;
        temp_file
            .as_file()
            .sync_all()
            .map_err(keystore_error("error syncing temp index file"))?;

        // Atomically replace the index file.
        temp_file
            .persist(&index_path)
            .map_err(|err| keystore_error("error renaming index file")(err.error))?;

        Ok(())
    }

    /// Returns the account ID associated with a given public key commitment hex.
    ///
    /// Iterates over all mappings to find which account contains the commitment.
    /// Returns `None` if no account is found.
    fn get_account_id(&self, pub_key_commitment: PublicKeyCommitment) -> Option<AccountId> {
        let pub_key_hex = Word::from(pub_key_commitment).to_hex();

        for (account_id_hex, commitments) in &self.mappings {
            if commitments.contains(&pub_key_hex) {
                return AccountId::from_hex(account_id_hex).ok();
            }
        }

        None
    }

    /// Gets all public key commitments for an account ID.
    fn get_commitments(
        &self,
        account_id: &AccountId,
    ) -> Result<BTreeSet<PublicKeyCommitment>, KeyStoreError> {
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
            .ok_or_else(|| {
                KeyStoreError::StorageError(format!("account not found {account_id_hex}"))
            })
    }
}

// FILESYSTEM KEYSTORE
// ================================================================================================

/// A filesystem-based keystore that stores keys in separate files and provides transaction
/// authentication functionality. The public key is hashed and the result is used as the filename
/// and the contents of the file are the serialized public and secret key.
///
/// Account-to-key mappings are stored in a separate JSON index file.
#[derive(Debug)]
pub struct FilesystemKeyStore {
    /// The directory where the keys are stored and read from.
    pub keys_directory: PathBuf,
    /// The in-memory index of account-to-key mappings.
    index: RwLock<KeyIndex>,
}

impl Clone for FilesystemKeyStore {
    fn clone(&self) -> Self {
        let index = self.index.read().clone();
        Self {
            keys_directory: self.keys_directory.clone(),
            index: RwLock::new(index),
        }
    }
}

impl FilesystemKeyStore {
    /// Creates a [`FilesystemKeyStore`] on a specific directory.
    pub fn new(keys_directory: PathBuf) -> Result<Self, KeyStoreError> {
        if !keys_directory.exists() {
            fs::create_dir_all(&keys_directory)
                .map_err(keystore_error("error creating keys directory"))?;
        }

        let index = KeyIndex::read_from_file(&keys_directory)?;

        Ok(FilesystemKeyStore {
            keys_directory,
            index: RwLock::new(index),
        })
    }

    /// Adds a secret key to the keystore without updating account mappings.
    ///
    /// This is an internal method. Use [`Keystore::add_key`] instead.
    fn add_key_without_account(&self, key: &AuthSecretKey) -> Result<(), KeyStoreError> {
        let pub_key_commitment = key.public_key().to_commitment();
        let file_path = key_file_path(&self.keys_directory, pub_key_commitment);
        write_secret_key_file(&file_path, key)
    }

    /// Retrieves a secret key from the keystore given the commitment of a public key.
    pub fn get_key_sync(
        &self,
        pub_key: PublicKeyCommitment,
    ) -> Result<Option<AuthSecretKey>, KeyStoreError> {
        let file_path = key_file_path(&self.keys_directory, pub_key);
        match fs::read(&file_path) {
            Ok(bytes) => {
                let key = AuthSecretKey::read_from_bytes(&bytes).map_err(|err| {
                    KeyStoreError::DecodingError(format!(
                        "error reading secret key from file: {err:?}"
                    ))
                })?;
                Ok(Some(key))
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(keystore_error("error reading secret key file")(e)),
        }
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

        let signature = secret_key.sign(message);

        Ok(signature)
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

        // Write the key file
        self.add_key_without_account(key)?;

        // Update the index
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
        index.get_commitments(account_id)
    }
}

// HELPERS
// ================================================================================================

/// Returns the file path that belongs to the public key commitment
fn key_file_path(keys_directory: &Path, pub_key_commitment: PublicKeyCommitment) -> PathBuf {
    let filename = Word::from(pub_key_commitment).to_hex();
    keys_directory.join(filename)
}

/// Writes an [`AuthSecretKey`] into a file with restrictive permissions (0600 on Unix).
#[cfg(unix)]
fn write_secret_key_file(file_path: &Path, key: &AuthSecretKey) -> Result<(), KeyStoreError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(file_path)
        .map_err(keystore_error("error writing secret key file"))?;
    file.write_all(&key.to_bytes())
        .map_err(keystore_error("error writing secret key file"))
}

/// Writes an [`AuthSecretKey`] into a file.
// TODO: on Windows, set restrictive ACLs to limit access to the current user.
#[cfg(not(unix))]
fn write_secret_key_file(file_path: &Path, key: &AuthSecretKey) -> Result<(), KeyStoreError> {
    fs::write(file_path, key.to_bytes()).map_err(keystore_error("error writing secret key file"))
}

fn keystore_error(context: &str) -> impl FnOnce(std::io::Error) -> KeyStoreError {
    move |err| KeyStoreError::StorageError(format!("{context}: {err:?}"))
}

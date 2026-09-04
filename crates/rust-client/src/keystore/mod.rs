use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;

use miden_protocol::account::AccountId;
use miden_protocol::account::auth::{AuthSecretKey, PublicKeyCommitment};
use miden_tx::auth::TransactionAuthenticator;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeyStoreError {
    #[error("storage error: {0}")]
    StorageError(String),
    #[error("decoding error: {0}")]
    DecodingError(String),
}

/// A trait for managing cryptographic keys and their association with accounts.
///
/// This trait extends [`TransactionAuthenticator`] to provide a unified interface for key storage,
/// retrieval, and account-key mapping.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait Keystore: TransactionAuthenticator {
    /// Adds a secret key to the keystore and associates it with the given account.
    ///
    /// A key can be associated with multiple accounts by calling this method multiple times.
    async fn add_key(
        &self,
        key: &AuthSecretKey,
        account_id: AccountId,
    ) -> Result<(), KeyStoreError>;

    /// Removes a key from the keystore by its public key commitment.
    ///
    /// This also removes all account associations for this key.
    async fn remove_key(&self, pub_key: PublicKeyCommitment) -> Result<(), KeyStoreError>;

    /// Retrieves a secret key by its public key commitment.
    ///
    /// Returns `Ok(None)` if the key is not found.
    async fn get_key(
        &self,
        pub_key: PublicKeyCommitment,
    ) -> Result<Option<AuthSecretKey>, KeyStoreError>;

    /// Returns all public key commitments associated with the given account ID.
    ///
    /// Returns an error if the account is not found.
    async fn get_account_key_commitments(
        &self,
        account_id: &AccountId,
    ) -> Result<BTreeSet<PublicKeyCommitment>, KeyStoreError>;

    /// Returns the account ID associated with a given public key commitment.
    ///
    /// Returns `Ok(None)` if no account is found for the commitment.
    async fn get_account_id_by_key_commitment(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<Option<AccountId>, KeyStoreError>;

    /// Returns all secret keys associated with the given account ID.
    ///
    /// This is a convenience method that calls `get_account_key_commitments` followed by `get_key`
    /// for each commitment.
    ///
    /// Returns an empty vector if the account has no associated keys. Returns an error if any key
    /// lookup fails.
    async fn get_keys_for_account(
        &self,
        account_id: &AccountId,
    ) -> Result<Vec<AuthSecretKey>, KeyStoreError> {
        let commitments = self.get_account_key_commitments(account_id).await?;
        let mut keys = Vec::with_capacity(commitments.len());
        for commitment in commitments {
            if let Some(key) = self.get_key(commitment).await? {
                keys.push(key);
            }
        }
        Ok(keys)
    }
}

#[cfg(feature = "std")]
mod fs_keystore;
#[cfg(feature = "std")]
pub use fs_keystore::FilesystemKeyStore;

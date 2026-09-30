use std::collections::BTreeSet;
use std::sync::Arc;

use miden_client::AuthenticationError;
use miden_client::account::AccountId;
use miden_client::auth::{
    AuthSecretKey,
    PublicKey,
    PublicKeyCommitment,
    Signature,
    SigningInputs,
    TransactionAuthenticator,
};
use miden_client::keystore::{
    EncryptedFilesystemKeyStore,
    FilesystemKeyStore,
    KeyStoreError,
    Keystore,
    StoredKeyInfo,
};

// CLI KEYSTORE
// ================================================================================================

/// The keystore of the CLI. The `keystore_encrypted` field of the configuration selects the
/// variant.
#[derive(Clone, Debug)]
pub enum CliKeyStore {
    Plaintext(FilesystemKeyStore),
    Encrypted(EncryptedFilesystemKeyStore),
}

impl CliKeyStore {
    /// Stores a secret key without associating it with an account.
    pub fn store_key(&self, key: &AuthSecretKey) -> Result<(), KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.store_key(key),
            Self::Encrypted(keystore) => keystore.store_key(key),
        }
    }

    /// Returns information about all secret keys in the keystore.
    pub fn list_keys(&self) -> Result<Vec<StoredKeyInfo>, KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.list_keys(),
            Self::Encrypted(keystore) => keystore.list_keys(),
        }
    }

    /// Associates a stored key with an account.
    pub fn associate_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
        account_id: AccountId,
    ) -> Result<(), KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.associate_key(pub_key_commitment, account_id),
            Self::Encrypted(keystore) => keystore.associate_key(pub_key_commitment, account_id),
        }
    }

    /// Removes the association between a stored key and an account. Returns `true` if the
    /// association was present.
    pub fn disassociate_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
        account_id: AccountId,
    ) -> Result<bool, KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.disassociate_key(pub_key_commitment, account_id),
            Self::Encrypted(keystore) => keystore.disassociate_key(pub_key_commitment, account_id),
        }
    }
}

impl TransactionAuthenticator for CliKeyStore {
    async fn get_signature(
        &self,
        pub_key_commitment: PublicKeyCommitment,
        signing_inputs: &SigningInputs,
    ) -> Result<Signature, AuthenticationError> {
        match self {
            Self::Plaintext(keystore) => {
                keystore.get_signature(pub_key_commitment, signing_inputs).await
            },
            Self::Encrypted(keystore) => {
                keystore.get_signature(pub_key_commitment, signing_inputs).await
            },
        }
    }

    async fn get_public_key(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Option<Arc<PublicKey>> {
        match self {
            Self::Plaintext(keystore) => keystore.get_public_key(pub_key_commitment).await,
            Self::Encrypted(keystore) => keystore.get_public_key(pub_key_commitment).await,
        }
    }
}

#[async_trait::async_trait]
impl Keystore for CliKeyStore {
    async fn add_key(
        &self,
        key: &AuthSecretKey,
        account_id: AccountId,
    ) -> Result<(), KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.add_key(key, account_id).await,
            Self::Encrypted(keystore) => keystore.add_key(key, account_id).await,
        }
    }

    async fn remove_key(&self, pub_key: PublicKeyCommitment) -> Result<(), KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.remove_key(pub_key).await,
            Self::Encrypted(keystore) => keystore.remove_key(pub_key).await,
        }
    }

    async fn get_key(
        &self,
        pub_key: PublicKeyCommitment,
    ) -> Result<Option<AuthSecretKey>, KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.get_key(pub_key).await,
            Self::Encrypted(keystore) => keystore.get_key(pub_key).await,
        }
    }

    async fn get_account_key_commitments(
        &self,
        account_id: &AccountId,
    ) -> Result<BTreeSet<PublicKeyCommitment>, KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => keystore.get_account_key_commitments(account_id).await,
            Self::Encrypted(keystore) => keystore.get_account_key_commitments(account_id).await,
        }
    }

    async fn get_account_id_by_key_commitment(
        &self,
        pub_key_commitment: PublicKeyCommitment,
    ) -> Result<Option<AccountId>, KeyStoreError> {
        match self {
            Self::Plaintext(keystore) => {
                keystore.get_account_id_by_key_commitment(pub_key_commitment).await
            },
            Self::Encrypted(keystore) => {
                keystore.get_account_id_by_key_commitment(pub_key_commitment).await
            },
        }
    }
}

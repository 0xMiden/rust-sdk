use alloc::string::String;

use miden_protocol::account::AccountId;
use miden_protocol::asset::AssetId;
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::mmr::MmrError;
use miden_protocol::errors::{
    AccountError,
    AccountIdError,
    AccountPatchError,
    AssetError,
    AssetVaultError,
    NoteError,
    StorageMapError,
};
use miden_protocol::utils::serde::DeserializationError;
use miden_protocol::{Word, WordError};
use miden_tx::DataStoreError;
use thiserror::Error;

// STORE ERROR
// ================================================================================================

/// Errors generated from the store.
#[derive(Debug, Error)]
#[allow(clippy::large_enum_variant)]
pub enum StoreError {
    #[error("protocol configuration {0} is not stored; sync the client to get it from the node")]
    ProtocolConfigNotFound(Word),
    #[error("stored protocol configuration does not match commitment {0}")]
    ProtocolConfigCommitmentMismatch(Word),
    #[error("asset error")]
    AssetError(#[from] AssetError),
    #[error("asset vault error")]
    AssetVaultError(#[from] AssetVaultError),
    #[error("account data wasn't found for account id {0}")]
    AccountDataNotFound(AccountId),
    #[error("account patch error")]
    AccountPatchError(#[from] AccountPatchError),
    #[error("account error")]
    AccountError(#[from] AccountError),
    #[error("invalid account ID")]
    AccountIdError(#[from] AccountIdError),
    #[error("stored account commitment does not match the expected commitment for account {0}")]
    AccountCommitmentMismatch(AccountId),
    #[error("account storage data with root {0} not found")]
    AccountStorageRootNotFound(Word),
    #[error("block header for block {0} not found")]
    BlockHeaderNotFound(BlockNumber),
    #[error("partial blockchain node at index {0} not found")]
    PartialBlockchainNodeNotFound(u64),
    #[error("failed to deserialize data from the store")]
    DataDeserializationError(#[from] DeserializationError),
    #[error("database-related non-query error: {0}")]
    DatabaseError(String),
    #[error("transient database error, the operation can be retried: {0}")]
    DatabaseTransientError(String),
    #[error("permanent database error: {0}")]
    DatabasePermanentError(String),
    #[error("merkle store error")]
    MerkleStoreError(#[from] MerkleError),
    #[error("failed to construct Merkle Mountain Range (MMR)")]
    MmrError(#[from] MmrError),
    #[error("failed to create note inclusion proof")]
    NoteInclusionProofError(#[from] NoteError),
    #[error("note script with root {0} not found")]
    NoteScriptNotFound(String),
    #[error("failed to parse data retrieved from the database: {0}")]
    ParsingError(String),
    #[error("failed to retrieve data from the database: {0}")]
    QueryError(String),
    #[error("account storage map error")]
    StorageMapError(#[from] StorageMapError),
    #[error("vault key {0:?} (hashed to {1}) is not tracked in the vault")]
    VaultKeyNotTracked(AssetId, Word),
    #[error("failed to parse word")]
    WordError(#[from] WordError),
}

impl From<StoreError> for DataStoreError {
    fn from(value: StoreError) -> Self {
        match value {
            StoreError::AccountDataNotFound(account_id) => {
                DataStoreError::AccountNotFound(account_id)
            },
            err => DataStoreError::other_with_source("store error", err),
        }
    }
}

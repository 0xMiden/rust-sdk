use alloc::string::String;
use core::num::TryFromIntError;

use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::mmr::MmrError;
use miden_protocol::crypto::merkle::smt::SmtProofError;
use miden_protocol::errors::{
    AccountDeltaError,
    AccountError,
    AccountIdError,
    AddressError,
    AssetError,
    AssetVaultError,
    NoteError,
    StorageMapError,
    TransactionScriptError,
};
use miden_protocol::utils::HexParseError;
use miden_protocol::utils::serde::DeserializationError;
use miden_protocol::{Word, WordError};
use miden_tx::DataStoreError;
use thiserror::Error;

use super::note_record::NoteRecordError;

// STORE ERROR
// ================================================================================================

/// Errors generated from the store.
#[derive(Debug, Error)]
#[allow(clippy::large_enum_variant)]
pub enum StoreError {
    #[error("asset error")]
    AssetError(#[from] AssetError),
    #[error("asset vault error")]
    AssetVaultError(#[from] AssetVaultError),
    #[error("account code data with root {0} not found")]
    AccountCodeDataNotFound(Word),
    #[error("account data wasn't found for account id {0}")]
    AccountDataNotFound(AccountId),
    #[error("account delta error")]
    AccountDeltaError(#[from] AccountDeltaError),
    #[error("account error")]
    AccountError(#[from] AccountError),
    #[error("address error")]
    AddressError(#[from] AddressError),
    #[error("invalid account ID")]
    AccountIdError(#[from] AccountIdError),
    #[error("stored account commitment does not match the expected commitment for account {0}")]
    AccountCommitmentMismatch(AccountId),
    #[error("account storage data with root {0} not found")]
    AccountStorageRootNotFound(Word),
    #[error("account storage data with index {0} not found")]
    AccountStorageIndexNotFound(usize),
    #[error("block header for block {0} not found")]
    BlockHeaderNotFound(BlockNumber),
    #[error("partial blockchain node at index {0} not found")]
    PartialBlockchainNodeNotFound(u64),
    #[error("failed to deserialize data from the store")]
    DataDeserializationError(#[from] DeserializationError),
    #[error("database-related non-query error: {0}")]
    DatabaseError(String),
    #[error("failed to parse hex value")]
    HexParseError(#[from] HexParseError),
    #[error("integer conversion failed")]
    InvalidInt(#[from] TryFromIntError),
    #[error("note record error")]
    NoteRecordError(#[from] NoteRecordError),
    #[error("merkle store error")]
    MerkleStoreError(#[from] MerkleError),
    #[error("failed to construct Merkle Mountain Range (MMR)")]
    MmrError(#[from] MmrError),
    #[error("failed to create note inclusion proof")]
    NoteInclusionProofError(#[from] NoteError),
    #[error("note tag {0} is already being tracked")]
    NoteTagAlreadyTracked(u64),
    #[error("note script with root {0} not found")]
    NoteScriptNotFound(String),
    #[error("failed to parse data retrieved from the database: {0}")]
    ParsingError(String),
    #[error("failed to retrieve data from the database: {0}")]
    QueryError(String),
    #[error("sparse merkle tree proof error")]
    SmtProofError(#[from] SmtProofError),
    #[error("account storage map error")]
    StorageMapError(#[from] StorageMapError),
    #[error("failed to instantiate transaction script")]
    TransactionScriptError(#[from] TransactionScriptError),
    #[error("account vault data for root {0} not found")]
    VaultDataNotFound(Word),
    #[error("failed to parse word")]
    WordError(#[from] WordError),
    #[error("operation `{0}` is not supported by this store backend")]
    UnsupportedOperation(&'static str),
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

use alloc::string::String;
use core::num::TryFromIntError;

use miden_protocol::account::AccountId;
use miden_protocol::asset::AssetId;
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::merkle::MerkleError;
use miden_protocol::crypto::merkle::mmr::MmrError;
use miden_protocol::crypto::merkle::smt::SmtProofError;
use miden_protocol::errors::{
    AccountDeltaError,
    AccountError,
    AccountIdError,
    AccountPatchError,
    AddressError,
    AssetError,
    AssetVaultError,
    NoteError,
    StorageMapError,
};
use miden_protocol::utils::HexParseError;
use miden_protocol::utils::serde::DeserializationError;
use miden_protocol::{MastForestScriptError, Word, WordError};
use miden_tx::DataStoreError;
use thiserror::Error;

use super::note_record::NoteRecordError;

// STALE UPDATE
// ================================================================================================

/// A write was rejected because the state it was derived from is no longer the state the store
/// holds.
///
/// Nothing was applied. Re-read the state and rebuild the update against it.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum StaleUpdate {
    #[error(
        "account {account_id} update was derived from commitment {initial_commitment}, which the store no longer holds (now {stored_commitment})"
    )]
    AccountCommitmentMismatch {
        account_id: AccountId,
        initial_commitment: Word,
        stored_commitment: Word,
    },
    #[error(
        "account {account_id} update at nonce {new_nonce} is not newer than the stored nonce {stored_nonce}"
    )]
    AccountNonceTooLow {
        account_id: AccountId,
        new_nonce: u64,
        stored_nonce: u64,
    },
    #[error(
        "input note {details_commitment} is stored in state {stored_discriminant}, which state {new_discriminant} would move backwards"
    )]
    InvalidInputNoteTransition {
        details_commitment: Word,
        stored_discriminant: u8,
        new_discriminant: u8,
    },
    #[error(
        "output note {details_commitment} is stored in state {stored_discriminant}, which state {new_discriminant} would move backwards"
    )]
    InvalidOutputNoteTransition {
        details_commitment: Word,
        stored_discriminant: u8,
        new_discriminant: u8,
    },
    #[error("block {0} still holds an unspent note")]
    Block(BlockNumber),
}

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
    #[error("account code data with root {0} not found")]
    AccountCodeDataNotFound(Word),
    #[error("account data wasn't found for account id {0}")]
    AccountDataNotFound(AccountId),
    #[error("account delta error")]
    AccountDeltaError(#[from] AccountDeltaError),
    #[error("account patch error")]
    AccountPatchError(#[from] AccountPatchError),
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
    #[error("transient database error, the operation can be retried: {0}")]
    DatabaseTransientError(String),
    #[error("permanent database error: {0}")]
    DatabasePermanentError(String),
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
    #[error("the store no longer holds the state this update was derived from")]
    StaleUpdate(#[from] StaleUpdate),
    #[error("account storage map error")]
    StorageMapError(#[from] StorageMapError),
    #[error("failed to instantiate a script from its mast forest")]
    MastForestScriptError(#[from] MastForestScriptError),
    #[error("account vault data for root {0} not found")]
    VaultDataNotFound(Word),
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

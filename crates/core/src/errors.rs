//! Defines the errors of the types shared by the client and the store implementations.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use core::error::Error as CoreError;

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
use miden_protocol::note::{NoteDetailsCommitment, NoteTag};
use miden_protocol::utils::serde::DeserializationError;
use miden_protocol::{Word, WordError};
use thiserror::Error;

mod rpc;
pub use rpc::*;

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
    #[error("note tags of account {0} are derived from its addresses and cannot be stored")]
    AccountNoteTagNotStorable(AccountId),
    #[error("account {0} has pending transactions and cannot be removed")]
    AccountHasPendingTransactions(AccountId),
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
    #[error("store {0} is locked by another store instance")]
    StoreLocked(String),
    #[error("vault key {0:?} (hashed to {1}) is not tracked in the vault")]
    VaultKeyNotTracked(AssetId, Word),
    #[error("failed to parse word")]
    WordError(#[from] WordError),
}

// NOTE RECORD ERROR
// ================================================================================================

/// Errors generated from note records.
#[derive(Debug, Error)]
pub enum NoteRecordError {
    /// Error generated during conversion of note record.
    #[error("note record conversion error: {0}")]
    ConversionError(String),
    /// Note record isn't consumable.
    #[error("note not consumable: {0}")]
    NoteNotConsumable(String),
    /// Invalid state transition.
    #[error("invalid state transition: {0}")]
    InvalidStateTransition(String),
    /// Error generated during a state transition.
    #[error("state transition error: {0}")]
    StateTransitionError(String),
}

impl From<NoteRecordError> for String {
    fn from(err: NoteRecordError) -> String {
        err.to_string()
    }
}

// ACCOUNT RECORD ERROR
// ================================================================================================

/// Errors generated when an [`AccountRecord`](crate::store::AccountRecord) does not hold the
/// requested kind of account data.
#[derive(Debug, Error)]
pub enum AccountRecordError {
    #[error("expected full account data for account {0}, but only partial data is available")]
    NotFull(AccountId),
    #[error("expected partial account data for account {0}, but full data was found")]
    NotPartial(AccountId),
}

// TRANSACTION ENCRYPTION ERROR
// ================================================================================================

/// Errors generated when a transaction encryption key is verified.
#[derive(Debug, Error)]
pub enum TransactionEncryptionError {
    #[error("{0}")]
    KeyRejected(String),
}

// NOTE TRANSPORT ERROR
// ================================================================================================

/// Errors generated by the note transport.
#[derive(Debug, Error)]
pub enum NoteTransportError {
    #[error(
        "note transport is disabled; enable it in the client configuration to send or receive notes via P2P"
    )]
    Disabled,
    #[error("connection error: {0}")]
    Connection(#[source] Box<dyn CoreError + Send + Sync + 'static>),
    #[error("deserialization error: {0}")]
    Deserialization(#[from] DeserializationError),
    #[error(
        "note details commitment {} does not match the header commitment {}",
        details.to_hex(),
        header.to_hex()
    )]
    NoteDetailsMismatch {
        header: NoteDetailsCommitment,
        details: NoteDetailsCommitment,
    },
    #[error("note transport returned an invalid note: {0}")]
    InvalidFetchedNote(#[source] ConversionError),
    #[error("note transport returned a note with tag {0}, which the client did not request")]
    UnrequestedTag(NoteTag),
    #[error("note transport network error: {0}")]
    Network(String),
}

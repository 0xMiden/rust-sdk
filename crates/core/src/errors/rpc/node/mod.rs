mod account;
mod block;
mod note;
mod sync;
mod transaction;

pub use account::{GetAccountError, RegisterAccountError};
pub use block::{GetBlockByNumberError, GetBlockHeaderError};
pub use note::{GetNoteScriptByRootError, GetNotesByIdError};
pub use sync::{
    NoteSyncError,
    SyncAccountStorageMapsError,
    SyncAccountVaultError,
    SyncChainMmrError,
    SyncNullifiersError,
    SyncTransactionsError,
};
use thiserror::Error;
pub use transaction::AddTransactionError;

/// Application-level error returned by the node for a specific RPC endpoint.
///
/// Each variant wraps a typed error decoded from the error code byte in the details of the node's
/// gRPC status.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EndpointError {
    /// Error from the `SubmitProvenTransaction` or `SubmitProvenBatch` endpoint
    #[error(transparent)]
    AddTransaction(#[from] AddTransactionError),
    /// Error from the `GetBlockHeaderByNumber` endpoint
    #[error(transparent)]
    GetBlockHeader(#[from] GetBlockHeaderError),
    /// Error from the `GetBlockByNumber` endpoint
    #[error(transparent)]
    GetBlockByNumber(#[from] GetBlockByNumberError),
    /// Error from the `SyncNotes` endpoint
    #[error(transparent)]
    NoteSync(#[from] NoteSyncError),
    /// Error from the `SyncNullifiers` endpoint
    #[error(transparent)]
    SyncNullifiers(#[from] SyncNullifiersError),
    /// Error from the `SyncAccountVault` endpoint
    #[error(transparent)]
    SyncAccountVault(#[from] SyncAccountVaultError),
    /// Error from the `SyncStorageMaps` endpoint
    #[error(transparent)]
    SyncStorageMaps(#[from] SyncAccountStorageMapsError),
    /// Error from the `SyncTransactions` endpoint
    #[error(transparent)]
    SyncTransactions(#[from] SyncTransactionsError),
    /// Error from the `SyncChainMmr` endpoint
    #[error(transparent)]
    SyncChainMmr(#[from] SyncChainMmrError),
    /// Error from the `GetNotesById` endpoint
    #[error(transparent)]
    GetNotesById(#[from] GetNotesByIdError),
    /// Error from the `GetNoteScriptByRoot` endpoint
    #[error(transparent)]
    GetNoteScriptByRoot(#[from] GetNoteScriptByRootError),
    /// Error from the `GetAccount` endpoint
    #[error(transparent)]
    GetAccount(#[from] GetAccountError),
    /// Error from the `RegisterAccount` endpoint
    #[error(transparent)]
    RegisterAccount(#[from] RegisterAccountError),
}

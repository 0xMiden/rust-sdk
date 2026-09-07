mod account;
mod block;
mod note;
mod sync;
mod transaction;

pub use account::GetAccountError;
pub use block::{GetBlockByNumberError, GetBlockHeaderError};
pub use note::{GetNoteScriptByRootError, GetNotesByIdError};
pub use sync::{
    NoteSyncError,
    SyncAccountStorageMapsError,
    SyncAccountVaultError,
    SyncNullifiersError,
    SyncTransactionsError,
};
use thiserror::Error;
pub use transaction::AddTransactionError;

use crate::rpc::RpcEndpoint;

/// Application-level error returned by the node for a specific RPC endpoint.
///
/// Each variant wraps a typed error parsed from the error code in the node's gRPC response.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EndpointError {
    /// Error from the `SubmitProvenTransaction` endpoint
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
    /// Error from the `GetNotesById` endpoint
    #[error(transparent)]
    GetNotesById(#[from] GetNotesByIdError),
    /// Error from the `GetNoteScriptByRoot` endpoint
    #[error(transparent)]
    GetNoteScriptByRoot(#[from] GetNoteScriptByRootError),
    /// Error from the `GetAccount` endpoint
    #[error(transparent)]
    GetAccount(#[from] GetAccountError),
}

/// Parses the application-level error code into a typed error for the given endpoint.
///
/// Returns `None` if details are empty or if the endpoint doesn't have typed errors.
pub fn parse_node_error(
    endpoint: &RpcEndpoint,
    details: &[u8],
    message: &str,
) -> Option<EndpointError> {
    let code = *details.first()?;

    match endpoint {
        RpcEndpoint::SubmitProvenTx => {
            Some(EndpointError::AddTransaction(AddTransactionError::from_code(code, message)))
        },
        RpcEndpoint::GetBlockHeaderByNumber => {
            Some(EndpointError::GetBlockHeader(GetBlockHeaderError::from_code(code, message)))
        },
        RpcEndpoint::GetBlockByNumber => {
            Some(EndpointError::GetBlockByNumber(GetBlockByNumberError::from_code(code, message)))
        },
        RpcEndpoint::SyncNotes => {
            Some(EndpointError::NoteSync(NoteSyncError::from_code(code, message)))
        },
        RpcEndpoint::SyncNullifiers => {
            Some(EndpointError::SyncNullifiers(SyncNullifiersError::from_code(code, message)))
        },
        RpcEndpoint::SyncAccountVault => {
            Some(EndpointError::SyncAccountVault(SyncAccountVaultError::from_code(code, message)))
        },
        RpcEndpoint::SyncStorageMaps => Some(EndpointError::SyncStorageMaps(
            SyncAccountStorageMapsError::from_code(code, message),
        )),
        RpcEndpoint::SyncTransactions => {
            Some(EndpointError::SyncTransactions(SyncTransactionsError::from_code(code, message)))
        },
        RpcEndpoint::GetNotesById => {
            Some(EndpointError::GetNotesById(GetNotesByIdError::from_code(code, message)))
        },
        RpcEndpoint::GetNoteScriptByRoot => Some(EndpointError::GetNoteScriptByRoot(
            GetNoteScriptByRootError::from_code(code, message),
        )),
        RpcEndpoint::GetAccount => {
            Some(EndpointError::GetAccount(GetAccountError::from_code(code, message)))
        },
        // These endpoints don't have typed errors from the node
        RpcEndpoint::SyncChainMmr
        | RpcEndpoint::Status
        | RpcEndpoint::GetLimits
        | RpcEndpoint::GetNetworkNoteStatus
        | RpcEndpoint::GetTransactionEncryptionKey
        | RpcEndpoint::SubmitProvenBatch => None,
    }
}

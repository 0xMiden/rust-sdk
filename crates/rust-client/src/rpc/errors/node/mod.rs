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

use crate::rpc::RpcEndpoint;
use crate::rpc::errors::GrpcError;

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
        RpcEndpoint::SyncChainMmr => {
            Some(EndpointError::SyncChainMmr(SyncChainMmrError::from_code(code, message)))
        },
        // These endpoints don't have typed errors from the node
        RpcEndpoint::Status
        | RpcEndpoint::GetLimits
        | RpcEndpoint::GetNetworkNoteStatus
        | RpcEndpoint::GetTransactionEncryptionKey
        | RpcEndpoint::RegisterAccount
        | RpcEndpoint::IsAccountAllowed
        | RpcEndpoint::SubmitProvenBatch => None,
    }
}

/// Parses the gRPC status code into a typed error for the given endpoint.
pub fn parse_status_error(
    endpoint: &RpcEndpoint,
    error_kind: &GrpcError,
    message: &str,
) -> Option<EndpointError> {
    // The match is exhaustive on purpose, so a new endpoint has to be classified before it
    // compiles.
    match endpoint {
        RpcEndpoint::RegisterAccount => RegisterAccountError::from_grpc_error(error_kind, message)
            .map(EndpointError::RegisterAccount),
        RpcEndpoint::SubmitProvenTx
        | RpcEndpoint::GetBlockHeaderByNumber
        | RpcEndpoint::GetBlockByNumber
        | RpcEndpoint::SyncNotes
        | RpcEndpoint::SyncNullifiers
        | RpcEndpoint::SyncAccountVault
        | RpcEndpoint::SyncStorageMaps
        | RpcEndpoint::SyncTransactions
        | RpcEndpoint::GetNotesById
        | RpcEndpoint::GetNoteScriptByRoot
        | RpcEndpoint::GetAccount
        | RpcEndpoint::SyncChainMmr
        | RpcEndpoint::Status
        | RpcEndpoint::GetLimits
        | RpcEndpoint::GetNetworkNoteStatus
        | RpcEndpoint::GetTransactionEncryptionKey
        | RpcEndpoint::IsAccountAllowed
        | RpcEndpoint::SubmitProvenBatch => None,
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// `RegisterAccount` is the only endpoint that classifies a status code into a typed error. The
    /// other endpoints must return `None` for the same code, so a rejection from one of them is
    /// never reported as a registration decision.
    #[test]
    fn only_the_register_account_endpoint_classifies_a_status_code() {
        let registration =
            parse_status_error(&RpcEndpoint::RegisterAccount, &GrpcError::NotFound, "unknown");

        assert!(matches!(
            registration,
            Some(EndpointError::RegisterAccount(RegisterAccountError::InvitationNotFound))
        ));

        for endpoint in [
            RpcEndpoint::GetAccount,
            RpcEndpoint::SubmitProvenTx,
            RpcEndpoint::GetNotesById,
            RpcEndpoint::Status,
        ] {
            assert!(
                parse_status_error(&endpoint, &GrpcError::NotFound, "unknown").is_none(),
                "{endpoint:?} must not classify a status code"
            );
        }
    }

    /// A transport failure on the registration endpoint carries no decision about the code, so it
    /// must stay unclassified.
    #[test]
    fn a_transport_failure_on_the_registration_endpoint_stays_unclassified() {
        assert!(
            parse_status_error(
                &RpcEndpoint::RegisterAccount,
                &GrpcError::Unavailable,
                "node is down"
            )
            .is_none()
        );
    }

    /// A request for a block after the chain tip must parse as `FutureBlock`, and a range that is
    /// invalid for another reason must stay `InvalidBlockRange`.
    #[test]
    fn a_block_after_the_chain_tip_parses_as_future_block() {
        assert_eq!(
            parse_node_error(
                &RpcEndpoint::SyncNotes,
                &[2],
                "block_to (5) is greater than chain tip (4)"
            ),
            Some(EndpointError::NoteSync(NoteSyncError::FutureBlock))
        );
        assert_eq!(
            parse_node_error(&RpcEndpoint::SyncChainMmr, &[2], "start block is not known"),
            Some(EndpointError::SyncChainMmr(SyncChainMmrError::FutureBlock))
        );
        assert_eq!(
            parse_node_error(
                &RpcEndpoint::SyncNullifiers,
                &[1],
                "block_to (5) is greater than chain tip (4)"
            ),
            Some(EndpointError::SyncNullifiers(SyncNullifiersError::FutureBlock))
        );
        assert_eq!(
            parse_node_error(&RpcEndpoint::SyncNullifiers, &[1], "invalid block range"),
            Some(EndpointError::SyncNullifiers(SyncNullifiersError::InvalidBlockRange))
        );
    }
}

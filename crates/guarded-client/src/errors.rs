use miden_client::ClientError;
use miden_protocol::account::AccountId;
use miden_tx::AuthenticationError;
use thiserror::Error;

use crate::{ExecutionFailure, MAX_DECLARED_BLOCKS};

/// Errors of a [`GuardedClient`](crate::GuardedClient).
#[derive(Debug, Error)]
pub enum GuardedClientError {
    /// The underlying client failed.
    #[error("client operation failed")]
    Client(#[from] ClientError),
    /// The client does not track the account.
    #[error("the client does not track account {0}")]
    AccountNotTracked(AccountId),
    /// The auth component of the account is not `AuthGuardedMultisig`.
    #[error("account {0} is not a guarded multisig account")]
    AccountNotGuarded(AccountId),
    /// The client has no authenticator to sign Guardian requests.
    #[error("the client has no authenticator to sign Guardian requests")]
    MissingAuthenticator,
    /// The authenticator does not hold the cosigner key.
    #[error("the authenticator does not hold the cosigner key")]
    CosignerKeyNotHeld,
    /// The cosigner key uses a signature scheme that Guardian does not read.
    #[error("the cosigner key uses a signature scheme that the client cannot send to Guardian")]
    UnsupportedKeyScheme,
    /// The authenticator failed to sign a Guardian request.
    #[error("failed to sign the Guardian request")]
    Signing(#[source] AuthenticationError),
    /// The auth args set no approval expiration. Guardian does not execute a proposal whose
    /// approval does not expire.
    #[error("the auth args set no approval expiration, so Guardian does not execute the proposal")]
    ApprovalExpirationMissing,
    /// The request declares more blocks than Guardian accepts.
    #[error("the request declares {0} blocks, and Guardian accepts at most {MAX_DECLARED_BLOCKS}")]
    TooManyDeclaredBlocks(usize),
    /// The transaction summary is for another account.
    #[error("the transaction summary is for account {0}, not for the account of the client")]
    SummaryAccountMismatch(AccountId),
    /// The transaction changes the Guardian of the account. Guardian does not execute it.
    #[error("the transaction changes the Guardian of the account, so the client must prove it")]
    GuardianSwitchExecutesLocally,
    /// The connection to Guardian failed.
    #[error("failed to connect to Guardian")]
    Connection(#[source] tonic::transport::Error),
    /// Guardian refused the request.
    #[error("Guardian refused the request ({code}): {message}")]
    Refused {
        code: String,
        message: String,
        retryable: bool,
        retry_after_secs: Option<u64>,
    },
    /// Guardian accepted the execution, and the execution failed.
    #[error("Guardian execution failed ({}): {}", .0.code, .0.message)]
    ExecutionFailed(ExecutionFailure),
    /// Guardian returned a response that the client cannot read.
    #[error("Guardian returned an invalid response: {0}")]
    InvalidResponse(String),
    /// The execution did not end before the timeout.
    #[error("the Guardian execution did not end before the timeout")]
    WaitTimedOut,
}

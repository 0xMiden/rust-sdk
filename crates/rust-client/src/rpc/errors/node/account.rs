use alloc::string::String;

use thiserror::Error;

use crate::rpc::errors::GrpcError;

// GET ACCOUNT ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::GetAccountError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetAccountError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Account was not found at the requested block
    #[error("account not found")]
    AccountNotFound,
    /// Account is not public
    #[error("account is not public")]
    AccountNotPublic,
    /// Requested block number is unknown
    #[error("unknown block")]
    UnknownBlock,
    /// Requested block has been pruned
    #[error("block pruned")]
    BlockPruned,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl GetAccountError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 => Self::DeserializationFailed,
            2 => Self::AccountNotFound,
            3 => Self::AccountNotPublic,
            4 => Self::UnknownBlock,
            5 => Self::BlockPruned,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// REGISTER ACCOUNT ERROR
// ================================================================================================

/// Reason the node rejected a registration.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum RegisterAccountError {
    #[error("invitation code does not exist")]
    InvitationNotFound,
    #[error("the invitation code or the account is already registered")]
    AlreadyRegistered,
    /// The request was malformed. The account ID was missing or unreadable, or the invitation code
    /// was empty.
    #[error("invalid registration request: {0}")]
    InvalidRequest(String),
}

impl RegisterAccountError {
    /// Returns the typed error for the status codes the node uses to reject a registration.
    ///
    /// Returns `None` for every other code, because those report a transport or node-side failure
    /// rather than a decision about the registration.
    pub fn from_grpc_error(error_kind: &GrpcError, message: &str) -> Option<Self> {
        match error_kind {
            GrpcError::NotFound => Some(Self::InvitationNotFound),
            GrpcError::AlreadyExists => Some(Self::AlreadyRegistered),
            GrpcError::InvalidArgument => Some(Self::InvalidRequest(String::from(message))),
            _ => None,
        }
    }
}

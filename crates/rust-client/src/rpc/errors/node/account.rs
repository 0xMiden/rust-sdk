use alloc::string::String;

use thiserror::Error;

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

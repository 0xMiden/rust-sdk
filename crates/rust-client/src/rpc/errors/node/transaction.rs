use alloc::string::String;

use thiserror::Error;

// Error codes match `miden-node/crates/block-producer/src/errors.rs::MempoolSubmissionError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AddTransactionError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Transaction has expired
    #[error("transaction expired")]
    Expired,
    /// Transaction conflicts with the current state
    #[error("transaction conflicts with current state: {message}")]
    StateConflict { message: String },
    /// Mempool is at capacity
    #[error("the mempool is at capacity")]
    CapacityExceeded,
    /// Error code not recognized by this client version. This can happen if the node
    /// is newer than the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl AddTransactionError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 => Self::Expired,
            2 => Self::StateConflict { message: String::from(message) },
            3 => Self::CapacityExceeded,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

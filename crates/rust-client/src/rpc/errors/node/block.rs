use alloc::string::String;

use thiserror::Error;

// GET BLOCK HEADER ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::GetBlockHeaderError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetBlockHeaderError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl GetBlockHeaderError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// GET BLOCK BY NUMBER ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::GetBlockByNumberError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetBlockByNumberError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl GetBlockByNumberError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 => Self::DeserializationFailed,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

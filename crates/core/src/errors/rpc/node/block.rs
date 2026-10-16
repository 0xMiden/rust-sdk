use alloc::string::String;

use thiserror::Error;

// GET BLOCK HEADER ERROR
// ================================================================================================

// Error codes match `internal_error` in `miden-node/crates/rpc/src/server/api/error_codes.rs`. The
// node has no method-specific codes for this endpoint.
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

// Error codes match `internal_error` in `miden-node/crates/rpc/src/server/api/error_codes.rs`. The
// node has no method-specific codes for this endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetBlockByNumberError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl GetBlockByNumberError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

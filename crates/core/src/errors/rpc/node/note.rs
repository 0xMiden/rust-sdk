use alloc::string::String;

use thiserror::Error;

// GET NOTES BY ID ERROR
// ================================================================================================

// Error codes match `GetNotesByIdErrorCode` in
// `miden-node/crates/rpc/src/server/api/error_codes.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetNotesByIdError {
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

impl GetNotesByIdError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 => Self::DeserializationFailed,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// GET NOTE SCRIPT BY ROOT ERROR
// ================================================================================================

// Error codes match `GetNoteScriptByRootErrorCode` in
// `miden-node/crates/rpc/src/server/api/error_codes.rs`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetNoteScriptByRootError {
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

impl GetNoteScriptByRootError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 => Self::DeserializationFailed,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

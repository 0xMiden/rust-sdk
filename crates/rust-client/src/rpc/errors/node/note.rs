use alloc::string::String;

use thiserror::Error;

// GET NOTES BY ID ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::GetNotesByIdError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetNotesByIdError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Note was not found
    #[error("note not found")]
    NoteNotFound,
    /// Note is not public
    #[error("note is not public")]
    NoteNotPublic,
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
            2 => Self::NoteNotFound,
            3 => Self::NoteNotPublic,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// GET NOTE SCRIPT BY ROOT ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::GetNoteScriptByRootError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GetNoteScriptByRootError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Script was not found
    #[error("script not found")]
    ScriptNotFound,
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
            2 => Self::ScriptNotFound,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

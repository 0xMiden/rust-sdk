use alloc::string::String;

use thiserror::Error;

/// Returns whether the message is the node message for a block range that ends after the chain tip.
///
/// The node sends the same error code for this case and for an invalid block range on these
/// endpoints. Only the message identifies this case.
// TODO: Use a dedicated error code when the node sends one for these endpoints.
fn is_range_beyond_tip(message: &str) -> bool {
    message.contains("greater than chain tip")
}

// NOTE SYNC ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::NoteSyncError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NoteSyncError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Invalid block range specified
    #[error("invalid block range")]
    InvalidBlockRange,
    /// Requested block is ahead of the node's chain tip. This can happen when the node is behind
    /// the network, so a retry can succeed.
    #[error("requested block is ahead of the node's chain tip")]
    FutureBlock,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl NoteSyncError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 => Self::InvalidBlockRange,
            2 => Self::FutureBlock,
            3 => Self::DeserializationFailed,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// SYNC NULLIFIERS ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::SyncNullifiersError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SyncNullifiersError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Invalid block range specified
    #[error("invalid block range")]
    InvalidBlockRange,
    /// Requested block is ahead of the node's chain tip. This can happen when the node is behind
    /// the network, so a retry can succeed.
    #[error("requested block is ahead of the node's chain tip")]
    FutureBlock,
    /// Invalid prefix length
    #[error("invalid prefix length")]
    InvalidPrefixLength,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl SyncNullifiersError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 if is_range_beyond_tip(message) => Self::FutureBlock,
            1 => Self::InvalidBlockRange,
            2 => Self::InvalidPrefixLength,
            3 => Self::DeserializationFailed,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// SYNC ACCOUNT VAULT ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::SyncAccountVaultError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SyncAccountVaultError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Invalid block range specified
    #[error("invalid block range")]
    InvalidBlockRange,
    /// Requested block is ahead of the node's chain tip. This can happen when the node is behind
    /// the network, so a retry can succeed.
    #[error("requested block is ahead of the node's chain tip")]
    FutureBlock,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Account is not public
    #[error("account is not public")]
    AccountNotPublic,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl SyncAccountVaultError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 if is_range_beyond_tip(message) => Self::FutureBlock,
            1 => Self::InvalidBlockRange,
            2 => Self::DeserializationFailed,
            3 => Self::AccountNotPublic,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// SYNC ACCOUNT STORAGE MAPS ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::SyncAccountStorageMapsError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SyncAccountStorageMapsError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Invalid block range specified
    #[error("invalid block range")]
    InvalidBlockRange,
    /// Requested block is ahead of the node's chain tip. This can happen when the node is behind
    /// the network, so a retry can succeed.
    #[error("requested block is ahead of the node's chain tip")]
    FutureBlock,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Account was not found
    #[error("account not found")]
    AccountNotFound,
    /// Account is not public
    #[error("account is not public")]
    AccountNotPublic,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl SyncAccountStorageMapsError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 if is_range_beyond_tip(message) => Self::FutureBlock,
            1 => Self::InvalidBlockRange,
            2 => Self::DeserializationFailed,
            3 => Self::AccountNotFound,
            4 => Self::AccountNotPublic,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// SYNC TRANSACTIONS ERROR
// ================================================================================================

// Error codes match `miden-node/crates/store/src/errors.rs::SyncTransactionsError`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SyncTransactionsError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Invalid block range specified
    #[error("invalid block range")]
    InvalidBlockRange,
    /// Requested block is ahead of the node's chain tip. This can happen when the node is behind
    /// the network, so a retry can succeed.
    #[error("requested block is ahead of the node's chain tip")]
    FutureBlock,
    /// Failed to deserialize data
    #[error("deserialization failed")]
    DeserializationFailed,
    /// Account was not found
    #[error("account not found")]
    AccountNotFound,
    /// Witness error
    #[error("witness error")]
    WitnessError,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl SyncTransactionsError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            1 if is_range_beyond_tip(message) => Self::FutureBlock,
            1 => Self::InvalidBlockRange,
            2 => Self::DeserializationFailed,
            3 => Self::AccountNotFound,
            4 => Self::WitnessError,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

// SYNC CHAIN MMR ERROR
// ================================================================================================

// Error codes match `miden-node/crates/rpc/src/server/api/error_codes.rs::SyncChainMmrErrorCode`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SyncChainMmrError {
    /// Internal server error (code 0)
    #[error("internal server error")]
    Internal,
    /// Requested block is ahead of the node's chain tip. This can happen when the node is behind
    /// the network, so a retry can succeed.
    #[error("requested block is ahead of the node's chain tip")]
    FutureBlock,
    /// Error code not recognized by this client version. This can happen if the node is newer than
    /// the client and has added new error variants.
    #[error("unknown error code {code}: {message}")]
    Unknown { code: u8, message: String },
}

impl SyncChainMmrError {
    pub fn from_code(code: u8, message: &str) -> Self {
        match code {
            0 => Self::Internal,
            2 => Self::FutureBlock,
            _ => Self::Unknown { code, message: String::from(message) },
        }
    }
}

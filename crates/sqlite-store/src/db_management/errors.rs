use std::string::{String, ToString};

use rusqlite::Error as RusqliteError;
use rusqlite_migration::Error as MigrationError;
use thiserror::Error;

// ERRORS
// ================================================================================================

/// Errors generated from the `SQLite` store.
#[derive(Debug, Error)]
pub enum SqliteStoreError {
    #[error("Database error: {0}")]
    Database(String),
    #[error("Migration error: {0}")]
    Migration(String),
    #[error(
        "stored schema at version {version} does not match the schema this client builds for that version (expected {expected}, found {actual})"
    )]
    SchemaDrift {
        version: u32,
        expected: String,
        actual: String,
    },
    #[error(
        "store is at schema version {found}, which is newer than the highest version this client supports ({supported})"
    )]
    SchemaTooNew { found: u32, supported: u32 },
    #[error(
        "migrating to schema version {version} produced a schema this client does not expect (expected {expected}, found {actual})"
    )]
    MigratedSchemaMismatch {
        version: u32,
        expected: String,
        actual: String,
    },
    #[error(
        "the database is not empty and does not record a schema version, so it was not created by this client and will not be migrated into a store"
    )]
    NotAClientStore,
    #[error("failed to back up the store to {backup} before migrating it: {reason}")]
    BackupFailed { backup: String, reason: String },
    #[error(
        "migrating the store failed and it could not be restored from its backup at {backup}: {reason}. The backup holds the store as it was before migrating"
    )]
    BackupRestoreFailed { backup: String, reason: String },
}

impl From<RusqliteError> for SqliteStoreError {
    fn from(err: RusqliteError) -> Self {
        SqliteStoreError::Database(err.to_string())
    }
}

impl From<MigrationError> for SqliteStoreError {
    /// Renders a migration failure without reproducing the migration script.
    fn from(err: MigrationError) -> Self {
        let message = match &err {
            MigrationError::RusqliteError {
                err: RusqliteError::SqlInputError { msg, .. },
                ..
            } => msg.clone(),
            MigrationError::RusqliteError { err, .. } => err.to_string(),
            MigrationError::ForeignKeyCheck(violations) => {
                format!(
                    "{} foreign key violation(s) after applying the migration",
                    violations.len()
                )
            },
            other => other.to_string(),
        };

        SqliteStoreError::Migration(message)
    }
}

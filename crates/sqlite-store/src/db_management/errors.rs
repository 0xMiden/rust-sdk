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
}

impl From<RusqliteError> for SqliteStoreError {
    fn from(err: RusqliteError) -> Self {
        SqliteStoreError::Database(err.to_string())
    }
}

impl From<MigrationError> for SqliteStoreError {
    fn from(err: MigrationError) -> Self {
        SqliteStoreError::Migration(describe_migration_error(&err))
    }
}

/// Renders a migration failure without reproducing the migration script.
pub fn describe_migration_error(err: &MigrationError) -> String {
    match err {
        MigrationError::RusqliteError { err, .. } => describe_sqlite_error(err),
        MigrationError::ForeignKeyCheck(violations) => {
            format!("{} foreign key violation(s) after applying the migration", violations.len())
        },
        other => other.to_string(),
    }
}

/// Renders a `SQLite` failure without reproducing the statement that caused it.
fn describe_sqlite_error(err: &RusqliteError) -> String {
    match err {
        RusqliteError::SqlInputError { msg, .. } => msg.clone(),
        other => other.to_string(),
    }
}

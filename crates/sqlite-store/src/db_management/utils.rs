use std::string::String;
use std::sync::LazyLock;
use std::vec::Vec;

use miden_client::store::StoreError;
use miden_protocol::crypto::hash::blake::{Blake3_256, Blake3Digest};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, Result, ToSql, Transaction, params};
use rusqlite_migration::{M, Migrations, SchemaVersion};

use super::errors::SqliteStoreError;
use crate::sql_error::SqlResultExt;

// MACROS
// ================================================================================================

/// Auxiliary macro which substitutes `$src` token by `$dst` expression.
#[macro_export]
macro_rules! subst {
    ($src:tt, $dst:expr_2021) => {
        $dst
    };
}

/// Generates a simple insert SQL statement with parameters for the provided table name and fields.
/// Supports optional conflict resolution (adding "| REPLACE" or "| IGNORE" at the end will generate
/// "OR REPLACE" and "OR IGNORE", correspondingly).
///
/// # Usage:
///
/// ```ignore
/// insert_sql!(users { id, first_name, last_name, age } | REPLACE);
/// ```
///
/// which generates:
/// ```sql
/// INSERT OR REPLACE INTO `users` (`id`, `first_name`, `last_name`, `age`) VALUES (?, ?, ?, ?)
/// ```
#[macro_export]
macro_rules! insert_sql {
    ($table:ident { $first_field:ident $(, $($field:ident),+)? $(,)? } $(| $on_conflict:expr)?) => {
        concat!(
            stringify!(INSERT $(OR $on_conflict)? INTO ),
            "`",
            stringify!($table),
            "` (`",
            stringify!($first_field),
            $($(concat!("`, `", stringify!($field))),+ ,)?
            "`) VALUES (",
            subst!($first_field, "?"),
            $($(subst!($field, ", ?")),+ ,)?
            ")"
        )
    };
}

// MIGRATIONS
// ================================================================================================

type Hash = Blake3Digest<32>;

const MIGRATION_SCRIPTS: [&str; 2] = [
    include_str!("../store.sql"),
    include_str!("../migrations/0002_input_notes_script_root_index.sql"),
];
static MIGRATION_HASHES: LazyLock<Vec<Hash>> = LazyLock::new(compute_migration_hashes);
static MIGRATIONS: LazyLock<Migrations> = LazyLock::new(prepare_migrations);

fn up(s: &'static str) -> M<'static> {
    M::up(s).foreign_key_check()
}

const DB_MIGRATION_HASH_FIELD: &str = "db-migration-hash";

/// Applies the migrations to the database.
pub fn apply_migrations(conn: &mut Connection) -> Result<(), SqliteStoreError> {
    let version_before = MIGRATIONS.current_version(conn)?;

    if let SchemaVersion::Inside(ver) = version_before {
        if !table_exists(&conn.transaction()?, "migrations")? {
            return Err(SqliteStoreError::MissingMigrationsTable);
        }

        let expected_hash = &*MIGRATION_HASHES[ver.get() - 1];

        let Ok(Some(actual_hash)) = get_migrations_value::<Vec<u8>>(conn, DB_MIGRATION_HASH_FIELD)
        else {
            return Err(SqliteStoreError::DatabaseError("Migration hash not found".to_owned()));
        };

        if &actual_hash[..] != expected_hash {
            return Err(SqliteStoreError::MigrationHashMismatch);
        }
    }

    MIGRATIONS.to_latest(conn)?;

    let version_after = MIGRATIONS.current_version(conn)?;

    if version_before != version_after {
        let new_hash = &*MIGRATION_HASHES[MIGRATION_HASHES.len() - 1];
        set_migrations_value(conn, DB_MIGRATION_HASH_FIELD, &new_hash)?;
    }

    Ok(())
}

fn prepare_migrations() -> Migrations<'static> {
    Migrations::new(MIGRATION_SCRIPTS.map(up).to_vec())
}

fn compute_migration_hashes() -> Vec<Hash> {
    let mut accumulator = Hash::default();
    MIGRATION_SCRIPTS
        .iter()
        .map(|sql| {
            let script_hash = Blake3_256::hash(preprocess_sql(sql).as_bytes());
            accumulator = Blake3_256::merge(&[accumulator, script_hash]);
            accumulator
        })
        .collect()
}

fn preprocess_sql(sql: &str) -> String {
    // TODO: We can also remove all comments here (need to analyze the SQL script in order to remove
    //       comments in string literals).
    remove_spaces(sql)
}

fn remove_spaces(str: &str) -> String {
    str.chars().filter(|chr| !chr.is_whitespace()).collect()
}

pub fn get_migrations_value<T: FromSql>(conn: &mut Connection, name: &str) -> Result<Option<T>> {
    conn.transaction()?
        .query_row("SELECT value FROM migrations WHERE name = $1", params![name], |row| row.get(0))
        .optional()
}

pub fn set_migrations_value<T: ToSql>(conn: &Connection, name: &str, value: &T) -> Result<()> {
    let count =
        conn.execute(insert_sql!(migrations { name, value } | REPLACE), params![name, value])?;

    debug_assert_eq!(count, 1);

    Ok(())
}

pub fn get_setting<T: FromSql>(conn: &mut Connection, name: &str) -> Result<Option<T>, StoreError> {
    conn.transaction()
        .into_store_error()?
        .query_row("SELECT value FROM settings WHERE name = $1", params![name], |row| row.get(0))
        .optional()
        .into_store_error()
}

pub fn set_setting<T: ToSql>(conn: &Connection, name: &str, value: &T) -> Result<()> {
    let count =
        conn.execute(insert_sql!(settings { name, value } | REPLACE), params![name, value])?;

    debug_assert_eq!(count, 1);

    Ok(())
}

pub fn remove_setting(conn: &Connection, name: &str) -> Result<(), StoreError> {
    let count = conn
        .execute("DELETE FROM settings WHERE name = $1", params![name])
        .into_store_error()?;

    debug_assert_eq!(count, 1);

    Ok(())
}

pub fn list_setting_keys(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT name FROM settings").into_store_error()?;
    stmt.query_map([], |row| row.get::<_, String>(0))
        .into_store_error()?
        .collect::<Result<Vec<String>, _>>()
        .into_store_error()
}

/// Checks if a table exists in the database.
pub fn table_exists(transaction: &Transaction, table_name: &str) -> rusqlite::Result<bool> {
    Ok(transaction
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = $1",
            params![table_name],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, OptionalExtension};

    use super::apply_migrations;

    /// Applying the migrations creates the input notes script root index, and reopening a
    /// database already at the latest version is accepted rather than rejected.
    #[test]
    fn migrations_create_script_root_index() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();
        apply_migrations(&mut conn).unwrap();

        let index_exists = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = $1",
                ["idx_input_notes_script_root"],
                |_| Ok(()),
            )
            .optional()
            .unwrap()
            .is_some();

        assert!(index_exists);
    }
}

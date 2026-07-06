use std::string::String;
use std::sync::LazyLock;
use std::vec::Vec;

use miden_client::store::StoreError;
use miden_protocol::crypto::hash::blake::{Blake3_256, Blake3Digest};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, Result, ToSql, params};
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

/// Domain separator mixed into every schema fingerprint so it cannot collide with unrelated hashes.
const SCHEMA_HASH_DOMAIN: &[u8] = b"miden-client-sqlite-schema-v1";

const MIGRATION_SCRIPTS: [&str; 1] = [include_str!("../store.sql")];
static MIGRATIONS: LazyLock<Migrations> = LazyLock::new(prepare_migrations);
static EXPECTED_SCHEMA_HASHES: LazyLock<Vec<Hash>> = LazyLock::new(compute_expected_schema_hashes);

fn up(s: &'static str) -> M<'static> {
    M::up(s).foreign_key_check()
}

/// Applies the migrations to the database.
///
/// For an existing database, the current schema is fingerprinted and compared against the
/// fingerprint expected for its migration version before any migration runs. This rejects a
/// database whose schema has drifted from what the migrations produce, regardless of how the drift
/// happened (manual DDL, a partially applied migration, or corruption): the expected fingerprint is
/// derived from the migrations compiled into the binary, not from a value stored in the database
/// that a tampered file could carry along.
pub fn apply_migrations(conn: &mut Connection) -> Result<(), SqliteStoreError> {
    let version_before = MIGRATIONS.current_version(conn)?;

    if let SchemaVersion::Inside(ver) = version_before {
        let actual_hash = schema_hash(conn)?;
        if actual_hash != EXPECTED_SCHEMA_HASHES[ver.get() - 1] {
            return Err(SqliteStoreError::SchemaHashMismatch);
        }
    }

    MIGRATIONS.to_latest(conn)?;

    Ok(())
}

fn prepare_migrations() -> Migrations<'static> {
    Migrations::new(MIGRATION_SCRIPTS.map(up).to_vec())
}

/// Computes the schema fingerprint expected after each migration by replaying the migrations on an
/// in-memory database. Index `i` holds the fingerprint after migrations `1..=i + 1` have run, which
/// is the fingerprint a database reports while its version is `i + 1`.
fn compute_expected_schema_hashes() -> Vec<Hash> {
    let mut conn =
        Connection::open_in_memory().expect("in-memory database creation should not fail");
    (1..=MIGRATION_SCRIPTS.len())
        .map(|version| {
            MIGRATIONS
                .to_version(&mut conn, version)
                .expect("replaying a migration on the reference database should not fail");
            schema_hash(&conn).expect("hashing the reference schema should not fail")
        })
        .collect()
}

/// Fingerprints the database's current schema.
///
/// The fingerprint covers every non-internal object in `sqlite_schema` (tables, indexes, views, and
/// triggers) by hashing its type, name, table name, and whitespace-normalized SQL. Entries are
/// ordered by type, name, and table name so the fingerprint does not depend on object creation
/// order. SQLite-internal objects (names starting with `sqlite_`) and auto-created indexes (whose
/// `sql` is `NULL`) are excluded.
fn schema_hash(conn: &Connection) -> Result<Hash> {
    let mut stmt = conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema \
         WHERE sql IS NOT NULL AND name NOT LIKE 'sqlite_%' \
         ORDER BY type, name, tbl_name",
    )?;
    let entries = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                normalize_sql(&row.get::<_, String>(3)?),
            ))
        })?
        .collect::<Result<Vec<_>>>()?;

    let mut buf = Vec::new();
    push_field(&mut buf, SCHEMA_HASH_DOMAIN);
    for (object_type, name, table_name, sql) in entries {
        push_field(&mut buf, object_type.as_bytes());
        push_field(&mut buf, name.as_bytes());
        push_field(&mut buf, table_name.as_bytes());
        push_field(&mut buf, sql.as_bytes());
    }

    Ok(Blake3_256::hash(&buf))
}

/// Appends a length-prefixed field to `buf` so that concatenating fields is unambiguous.
fn push_field(buf: &mut Vec<u8>, field: &[u8]) {
    buf.extend_from_slice(&(field.len() as u64).to_le_bytes());
    buf.extend_from_slice(field);
}

/// Collapses runs of whitespace to single spaces and trims a trailing semicolon so cosmetic
/// differences in stored SQL text do not change the fingerprint.
fn normalize_sql(sql: &str) -> String {
    sql.trim_end()
        .trim_end_matches(';')
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::{
        EXPECTED_SCHEMA_HASHES,
        MIGRATION_SCRIPTS,
        apply_migrations,
        prepare_migrations,
        schema_hash,
    };
    use crate::db_management::errors::SqliteStoreError;

    #[test]
    fn honest_database_reopens_without_error() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();
        // Reopening a database already at the latest version fingerprints its schema and must
        // accept it.
        apply_migrations(&mut conn).unwrap();
    }

    #[test]
    fn schema_drift_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();

        // A change made outside the migrations, e.g. a manual `ALTER TABLE` run against the file.
        conn.execute("ALTER TABLE input_notes ADD COLUMN injected TEXT", []).unwrap();

        let err = apply_migrations(&mut conn).unwrap_err();
        assert!(matches!(err, SqliteStoreError::SchemaHashMismatch));
    }

    #[test]
    fn stale_migrations_row_does_not_affect_acceptance() {
        // An older database carries a `db-migration-hash` row in the `migrations` table. The guard
        // reads the live schema, not that row, so the row's presence is irrelevant.
        let mut conn = Connection::open_in_memory().unwrap();
        prepare_migrations().to_version(&mut conn, MIGRATION_SCRIPTS.len()).unwrap();
        conn.execute(
            "INSERT INTO migrations (name, value) VALUES ('db-migration-hash', x'00')",
            [],
        )
        .unwrap();

        apply_migrations(&mut conn).unwrap();
    }

    #[test]
    fn schema_hash_ignores_object_creation_order() {
        let left = Connection::open_in_memory().unwrap();
        left.execute_batch(
            "CREATE TABLE a (id INTEGER PRIMARY KEY);
             CREATE TABLE b (id INTEGER PRIMARY KEY);",
        )
        .unwrap();

        let right = Connection::open_in_memory().unwrap();
        right
            .execute_batch(
                "CREATE TABLE b (id INTEGER PRIMARY KEY);
             CREATE TABLE a (id INTEGER PRIMARY KEY);",
            )
            .unwrap();

        assert_eq!(schema_hash(&left).unwrap(), schema_hash(&right).unwrap());
    }

    #[test]
    fn expected_schema_hash_per_migration() {
        assert_eq!(EXPECTED_SCHEMA_HASHES.len(), MIGRATION_SCRIPTS.len());
    }
}

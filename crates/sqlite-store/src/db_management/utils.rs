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

const SCHEMA_HASH_DOMAIN: &[u8] = b"miden-client-sqlite-schema-v1";

/// The migrations that build the store schema, in the order they are applied.
const MIGRATION_SCRIPTS: [&str; 1] = [include_str!("../migrations/0001_init.sql")];

static MIGRATIONS: LazyLock<Migrations> = LazyLock::new(prepare_migrations);

/// The schema fingerprint each migration in [`MIGRATION_SCRIPTS`] produces, obtained by replaying
/// the migrations rather than by trusting a recorded value.
pub(crate) static EXPECTED_SCHEMA_HASHES: LazyLock<Vec<Hash>> =
    LazyLock::new(compute_expected_schema_hashes);

fn up(s: &'static str) -> M<'static> {
    M::up(s).foreign_key_check()
}

/// Returns whether the database holds a schema that is behind the latest version.
///
/// A database with no schema at all is not behind: there is nothing in it to preserve, so opening
/// it builds the latest schema directly.
pub fn has_pending_migrations(conn: &Connection) -> Result<bool, SqliteStoreError> {
    match MIGRATIONS.current_version(conn)? {
        SchemaVersion::Inside(ver) => Ok(ver.get() < MIGRATION_SCRIPTS.len()),
        // A version beyond the last migration is rejected when migrating, not backed up.
        SchemaVersion::NoneSet | SchemaVersion::Outside(_) => Ok(false),
    }
}

/// Brings the database up to the latest schema version, creating it if it is empty.
pub fn apply_migrations(conn: &mut Connection) -> Result<(), SqliteStoreError> {
    apply_migrations_with(conn, &MIGRATIONS, &EXPECTED_SCHEMA_HASHES)
}

/// [`apply_migrations`] with the migration set and its fingerprints injected, so that tests can
/// drive the paths a single migration cannot reach on its own.
pub(crate) fn apply_migrations_with(
    conn: &mut Connection,
    migrations: &Migrations,
    expected_schema_hashes: &[Hash],
) -> Result<(), SqliteStoreError> {
    let latest_version = expected_schema_hashes.len();

    match migrations.current_version(conn)? {
        SchemaVersion::NoneSet => {
            if !is_empty_database(conn)? {
                return Err(SqliteStoreError::NotAClientStore);
            }
        },
        SchemaVersion::Inside(ver) => {
            let expected = expected_schema_hashes[ver.get() - 1];
            let actual = schema_hash(conn)?;
            if actual != expected {
                return Err(SqliteStoreError::SchemaDrift {
                    version: schema_version(ver.get()),
                    expected: String::from(expected),
                    actual: String::from(actual),
                });
            }
        },
        SchemaVersion::Outside(ver) => {
            return Err(SqliteStoreError::SchemaTooNew {
                found: schema_version(ver.get()),
                supported: schema_version(latest_version),
            });
        },
    }

    migrations.to_latest(conn)?;

    verify_migrated_schema(conn, expected_schema_hashes, latest_version)
}

/// Returns whether the database holds no objects of its own.
fn is_empty_database(conn: &Connection) -> Result<bool, SqliteStoreError> {
    let objects: u32 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;

    Ok(objects == 0)
}

/// Checks that migrating to `version` built the schema that version is defined to build.
fn verify_migrated_schema(
    conn: &Connection,
    expected_schema_hashes: &[Hash],
    version: usize,
) -> Result<(), SqliteStoreError> {
    let expected = expected_schema_hashes[version - 1];
    let actual = schema_hash(conn)?;

    if actual != expected {
        return Err(SqliteStoreError::MigratedSchemaMismatch {
            version: schema_version(version),
            expected: String::from(expected),
            actual: String::from(actual),
        });
    }

    Ok(())
}

/// Narrows a migration index to the width schema versions are reported in.
///
/// `SQLite` stores the version in `PRAGMA user_version`, which is an `i32`, so a version that does
/// not fit is unreachable.
fn schema_version(version: usize) -> u32 {
    u32::try_from(version).expect("schema version should fit in a u32")
}

fn prepare_migrations() -> Migrations<'static> {
    Migrations::new(MIGRATION_SCRIPTS.map(up).to_vec())
}

/// Computes the schema fingerprint each migration produces by replaying the migrations on an
/// in-memory database.
pub(crate) fn compute_expected_schema_hashes_for(
    migrations: &Migrations,
    migration_count: usize,
) -> Vec<Hash> {
    let mut conn =
        Connection::open_in_memory().expect("in-memory database creation should not fail");
    (1..=migration_count)
        .map(|version| {
            migrations
                .to_version(&mut conn, version)
                .expect("replaying a migration on the reference database should not fail");
            schema_hash(&conn).expect("hashing the reference schema should not fail")
        })
        .collect()
}

fn compute_expected_schema_hashes() -> Vec<Hash> {
    compute_expected_schema_hashes_for(&MIGRATIONS, MIGRATION_SCRIPTS.len())
}

/// Fingerprints the database's current schema.
///
/// Entries are ordered by type, name, and table name so the fingerprint does not depend on object
/// creation order.
pub(crate) fn schema_hash(conn: &Connection) -> Result<Hash> {
    let mut stmt = conn.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema \
         WHERE sql IS NOT NULL AND name NOT GLOB 'sqlite_*' \
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

/// Appends a length-prefixed field to `buf` so that concatenating different field sequences can
/// never produce the same output.
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
        schema_hash,
        verify_migrated_schema,
    };
    use crate::db_management::errors::SqliteStoreError;

    const PINNED_SCHEMA_HASHES: [&str; MIGRATION_SCRIPTS.len()] =
        ["0x749fba4988cae911b43dd2a3efef634ce5f514515ae26687f791fb17612c5b7a"];

    #[test]
    fn honest_database_reopens_without_error() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();
        // Reopening a database already at the latest version fingerprints its schema and must
        // accept it.
        apply_migrations(&mut conn).unwrap();
    }

    #[test]
    fn fresh_database_is_built_to_the_latest_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();

        let version: usize = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, MIGRATION_SCRIPTS.len());
        assert_eq!(schema_hash(&conn).unwrap(), EXPECTED_SCHEMA_HASHES[version - 1]);
    }

    #[test]
    fn unversioned_database_with_contents_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        // A database that is not a store, named by mistake. It records no version, which is what
        // an empty file also looks like.
        conn.execute_batch("CREATE TABLE somebody_elses (id INTEGER PRIMARY KEY);")
            .unwrap();

        let err = apply_migrations(&mut conn).unwrap_err();
        assert!(
            matches!(err, SqliteStoreError::NotAClientStore),
            "a foreign database should not be migrated into a store, got {err:?}"
        );

        // Refusing must leave the database alone.
        let version: usize = conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
        assert_eq!(version, 0);
        let tables: u32 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_schema WHERE name = 'input_notes'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(tables, 0);
    }

    #[test]
    fn schema_drift_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();

        // A change made outside the migrations, e.g. a manual `ALTER TABLE` run against the file.
        conn.execute("ALTER TABLE input_notes ADD COLUMN injected TEXT", []).unwrap();

        let err = apply_migrations(&mut conn).unwrap_err();
        let SqliteStoreError::SchemaDrift { version, expected, actual } = err else {
            panic!("drifted schema should be reported as drift, got {err:?}");
        };
        assert_eq!(version, 1);
        assert_ne!(expected, actual);
    }

    #[test]
    fn database_from_a_newer_client_is_rejected() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();

        // A version this client has no migration for, as written by a later release.
        let ahead = MIGRATION_SCRIPTS.len() + 3;
        conn.pragma_update(None, "user_version", ahead).unwrap();

        let err = apply_migrations(&mut conn).unwrap_err();
        let SqliteStoreError::SchemaTooNew { found, supported } = err else {
            panic!("a database from a newer client should be reported as too new, got {err:?}");
        };
        assert_eq!(found as usize, ahead);
        assert_eq!(supported as usize, MIGRATION_SCRIPTS.len());
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
    fn migrated_schema_is_verified() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply_migrations(&mut conn).unwrap();

        // Migrating cannot be made to build the wrong schema without a broken migration, so the
        // schema is changed under the check instead, which is what such a migration would leave
        // behind.
        conn.execute("DROP TABLE input_notes", []).unwrap();

        let err = verify_migrated_schema(&conn, &EXPECTED_SCHEMA_HASHES, MIGRATION_SCRIPTS.len())
            .unwrap_err();
        let SqliteStoreError::MigratedSchemaMismatch { version, expected, actual } = err else {
            panic!("an unexpected migrated schema should be reported as a mismatch, got {err:?}");
        };
        assert_eq!(version as usize, MIGRATION_SCRIPTS.len());
        assert_ne!(expected, actual);
    }

    #[test]
    fn migration_schema_hashes_are_stable() {
        let replayed = EXPECTED_SCHEMA_HASHES.iter().copied().map(String::from).collect::<Vec<_>>();
        let pinned = PINNED_SCHEMA_HASHES.map(str::to_string).to_vec();

        assert_eq!(
            replayed, pinned,
            "a released migration builds a different schema than it did when it was pinned. \
             Append a new migration instead of editing an existing one. If this is a new \
             migration, append its hash rather than rewriting the entries before it."
        );
    }
}

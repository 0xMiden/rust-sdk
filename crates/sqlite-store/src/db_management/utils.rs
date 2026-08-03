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

/// The schema fingerprint each migration in [`MIGRATION_SCRIPTS`] produced when it was released.
const PINNED_SCHEMA_HASHES: [&str; MIGRATION_SCRIPTS.len()] =
    ["0x6300110a9f3efa3476fac4e736f94c33e07935ab7eedf357b38a50f55cabf140"];

static MIGRATIONS: LazyLock<Migrations> = LazyLock::new(prepare_migrations);

fn up(s: &'static str) -> M<'static> {
    M::up(s).foreign_key_check()
}

/// Brings the database up to the latest schema version, creating it if it is empty.
pub fn apply_migrations(conn: &mut Connection) -> Result<(), SqliteStoreError> {
    match MIGRATIONS.current_version(conn)? {
        SchemaVersion::NoneSet => {},
        SchemaVersion::Inside(ver) => {
            let expected = PINNED_SCHEMA_HASHES[ver.get() - 1];
            let actual = String::from(schema_hash(conn)?);
            if actual != expected {
                return Err(SqliteStoreError::SchemaDrift {
                    version: schema_version(ver.get()),
                    expected: expected.to_string(),
                    actual,
                });
            }
        },
        SchemaVersion::Outside(ver) => {
            return Err(SqliteStoreError::SchemaTooNew {
                found: schema_version(ver.get()),
                supported: schema_version(MIGRATION_SCRIPTS.len()),
            });
        },
    }

    MIGRATIONS.to_latest(conn)?;

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
#[cfg(test)]
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
/// Entries are ordered by type, name, and table name so the fingerprint does not depend on object
/// creation order.
fn schema_hash(conn: &Connection) -> Result<Hash> {
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

/// Rewrites the SQL text stored for a schema object into a form that ignores differences `SQLite`
/// itself ignores, so cosmetic edits do not change the fingerprint.
fn normalize_sql(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            // A doubled quote inside a quoted region escapes itself and does not close it.
            '\'' | '"' | '`' => {
                out.push(ch);
                while let Some(inner) = chars.next() {
                    out.push(inner);
                    if inner == ch {
                        if chars.peek() == Some(&ch) {
                            out.push(ch);
                            chars.next();
                        } else {
                            break;
                        }
                    }
                }
            },
            // Bracketed identifiers do not nest and have no escape sequence.
            '[' => {
                out.push(ch);
                for inner in chars.by_ref() {
                    out.push(inner);
                    if inner == ']' {
                        break;
                    }
                }
            },
            // A comment collapses to a separator rather than to nothing, because `SQLite` does not
            // require whitespace before `--` and fusing the tokens on either side of it would
            // change what the text means.
            '-' if chars.peek() == Some(&'-') => {
                chars.next();
                while chars.peek().is_some_and(|&inner| inner != '\n') {
                    chars.next();
                }
                push_separator(&mut out);
            },
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = '\0';
                for inner in chars.by_ref() {
                    if prev == '*' && inner == '/' {
                        break;
                    }
                    prev = inner;
                }
                push_separator(&mut out);
            },
            _ if is_sql_whitespace(ch) => push_separator(&mut out),
            _ => out.push(ch),
        }
    }

    let normalized = out.trim_end().trim_end_matches(';').trim();
    normalized.to_string()
}

/// Appends a single space unless one is already there, so adjacent separators do not stack up.
fn push_separator(out: &mut String) {
    if !out.is_empty() && !out.ends_with(' ') {
        out.push(' ');
    }
}

/// Returns whether `ch` separates tokens for `SQLite`.
///
/// This is deliberately narrower than [`char::is_whitespace`]. `SQLite` treats every byte above
/// the ASCII range as part of an identifier, so a Unicode space between two tokens makes them one
/// token and must not be normalized away.
fn is_sql_whitespace(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | '\r' | '\u{0c}')
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
        MIGRATION_SCRIPTS,
        PINNED_SCHEMA_HASHES,
        apply_migrations,
        compute_expected_schema_hashes,
        schema_hash,
    };
    use crate::db_management::errors::SqliteStoreError;

    fn hash_of(schema: &str) -> super::Hash {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema).unwrap();
        schema_hash(&conn).unwrap()
    }

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
    fn migration_schema_hashes_are_stable() {
        let replayed = compute_expected_schema_hashes()
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>();
        let pinned = PINNED_SCHEMA_HASHES.map(str::to_string).to_vec();

        assert_eq!(
            replayed, pinned,
            "a released migration builds a different schema than it did when it was pinned. \
             Append a new migration instead of editing an existing one. If this is a new \
             migration, append its hash rather than rewriting the entries before it."
        );
    }

    #[test]
    fn schema_hash_ignores_comment_edits() {
        let documented = hash_of(
            "CREATE TABLE items (
                 id INTEGER PRIMARY KEY, -- the identifier
                 /* the payload */
                 value TEXT
             );",
        );
        let reworded = hash_of(
            "CREATE TABLE items (
                 id INTEGER PRIMARY KEY, -- a completely different explanation
                 value TEXT
             );",
        );

        assert_eq!(documented, reworded);
    }
}

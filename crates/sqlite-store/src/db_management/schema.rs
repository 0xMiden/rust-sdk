use std::fmt;
use std::string::String;
use std::vec::Vec;

use miden_protocol::crypto::hash::blake::{Blake3_256, Blake3Digest};
use rusqlite::Connection;

use super::errors::SqliteStoreError;

// SCHEMA HASH
// ================================================================================================

/// Separates schema fingerprints from anything else this client hashes.
const SCHEMA_HASH_DOMAIN: &[u8] = b"miden-client-sqlite-schema-v1";

/// A fingerprint of every object in an `SQLite` database's schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SchemaHash(Blake3Digest<32>);

impl SchemaHash {
    /// Fingerprints the schema `conn` currently holds.
    ///
    /// Entries are ordered by type, name, and table name so the fingerprint does not depend on
    /// object creation order.
    pub(crate) fn of(conn: &Connection) -> Result<Self, SqliteStoreError> {
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
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut buf = Vec::new();
        push_field(&mut buf, SCHEMA_HASH_DOMAIN);
        for (object_type, name, table_name, sql) in entries {
            push_field(&mut buf, object_type.as_bytes());
            push_field(&mut buf, name.as_bytes());
            push_field(&mut buf, table_name.as_bytes());
            push_field(&mut buf, sql.as_bytes());
        }

        Ok(Self(Blake3_256::hash(&buf)))
    }
}

impl fmt::Display for SchemaHash {
    /// Renders the fingerprint as the `0x`-prefixed hex the pinned fingerprints are written in.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&String::from(self.0))
    }
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

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::SchemaHash;

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

        assert_eq!(SchemaHash::of(&left).unwrap(), SchemaHash::of(&right).unwrap());
    }
}

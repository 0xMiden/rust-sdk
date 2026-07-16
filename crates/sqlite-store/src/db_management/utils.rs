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
    include_str!("../migrations/0002_prune_output_note_tags.sql"),
];
static MIGRATION_HASHES: LazyLock<Vec<Hash>> = LazyLock::new(compute_migration_hashes);
static MIGRATIONS: LazyLock<Migrations> = LazyLock::new(prepare_migrations);

/// Builds the migration for `MIGRATION_SCRIPTS[index]`. The cumulative hash is written by a
/// hook inside the migration's own transaction, so the schema version and the stored hash can
/// never diverge (an interrupted upgrade rolls both back together).
fn migration(index: usize) -> M<'static> {
    let hash = (*MIGRATION_HASHES[index]).to_vec();
    M::up_with_hook(MIGRATION_SCRIPTS[index], move |tx: &Transaction| {
        set_migrations_value(tx, DB_MIGRATION_HASH_FIELD, &hash)?;
        Ok(())
    })
    .foreign_key_check()
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

    Ok(())
}

fn prepare_migrations() -> Migrations<'static> {
    Migrations::new((0..MIGRATION_SCRIPTS.len()).map(migration).collect())
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
    use std::vec;
    use std::vec::Vec;

    use miden_client::sync::NoteTagSource;
    use miden_client::utils::Serializable;
    use miden_protocol::EMPTY_WORD;
    use miden_protocol::crypto::hash::rpo::Rpo256;
    use miden_protocol::note::NoteDetailsCommitment;
    use rusqlite::{Connection, params};
    use rusqlite_migration::Migrations;

    use super::{
        DB_MIGRATION_HASH_FIELD,
        MIGRATION_HASHES,
        apply_migrations,
        get_migrations_value,
        migration,
    };

    fn note_source_bytes(commitment: &NoteDetailsCommitment) -> Vec<u8> {
        NoteTagSource::Note(*commitment).to_bytes()
    }

    fn insert_tag(conn: &Connection, source: &[u8]) {
        conn.execute("INSERT INTO tags (tag, source) VALUES (?, ?)", params![vec![0u8; 4], source])
            .unwrap();
    }

    fn insert_output_note(conn: &Connection, commitment: &NoteDetailsCommitment) {
        conn.execute(
            "INSERT INTO output_notes (details_commitment, note_id, recipient_digest, assets, \
             metadata, expected_height, state_discriminant, state, attachments) \
             VALUES (?, ?, '0xrecipient', x'00', x'00', 0, 0, x'00', x'00')",
            params![commitment.to_hex(), commitment.to_hex()],
        )
        .unwrap();
    }

    fn insert_input_note(
        conn: &Connection,
        commitment: &NoteDetailsCommitment,
        state_discriminant: u8,
    ) {
        conn.execute(
            "INSERT OR IGNORE INTO notes_scripts (script_root, serialized_note_script) \
             VALUES ('0xscript', x'00')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO input_notes (details_commitment, assets, attachments, serial_number, \
             inputs, script_root, state_discriminant, state, created_at) \
             VALUES (?, x'00', x'00', x'00', x'00', '0xscript', ?, x'00', 0)",
            params![commitment.to_hex(), state_discriminant],
        )
        .unwrap();
    }

    /// The prune migration deletes output-note tags (and duplicates) but keeps tags still
    /// needed by pre-inclusion input notes, input-only tags, and non-`Note`-sourced tags.
    #[test]
    fn migration_prunes_output_note_tags() {
        let mut conn = Connection::open_in_memory().unwrap();

        // Bring the database to schema version 1 (pre-prune), the way apply_migrations
        // would have left a store created before the prune migration existed. The hook must
        // leave the stored hash consistent with the version at every step.
        Migrations::new(vec![migration(0)]).to_latest(&mut conn).unwrap();
        let stored_hash: Vec<u8> =
            get_migrations_value(&mut conn, DB_MIGRATION_HASH_FIELD).unwrap().unwrap();
        assert_eq!(&stored_hash[..], &*MIGRATION_HASHES[0]);

        let word = |seed: &[u8]| Rpo256::hash(seed);
        // Output note with no input-note counterpart: its tag must be pruned.
        let output_only = NoteDetailsCommitment::from_raw_commitments(EMPTY_WORD, word(b"a"));
        // Self-directed note whose input record is still Expected: tag must be kept.
        let self_directed = NoteDetailsCommitment::from_raw_commitments(EMPTY_WORD, word(b"b"));
        // Self-directed note whose input record already committed: tag must be pruned.
        let self_committed = NoteDetailsCommitment::from_raw_commitments(EMPTY_WORD, word(b"c"));
        // Imported expected note with no output record: tag must be kept.
        let imported_input = NoteDetailsCommitment::from_raw_commitments(EMPTY_WORD, word(b"d"));

        insert_output_note(&conn, &output_only);
        insert_tag(&conn, &note_source_bytes(&output_only));

        insert_output_note(&conn, &self_directed);
        insert_input_note(&conn, &self_directed, 0); // Expected
        insert_tag(&conn, &note_source_bytes(&self_directed));

        insert_output_note(&conn, &self_committed);
        insert_input_note(&conn, &self_committed, 2); // Committed
        insert_tag(&conn, &note_source_bytes(&self_committed));

        // Inserted twice: the migration must also collapse duplicate rows.
        insert_input_note(&conn, &imported_input, 0); // Expected
        insert_tag(&conn, &note_source_bytes(&imported_input));
        insert_tag(&conn, &note_source_bytes(&imported_input));

        // Account-sourced tag (discriminant 0): must never be touched.
        let account_source = vec![0u8; 16];
        insert_tag(&conn, &account_source);

        apply_migrations(&mut conn).unwrap();

        // The stored hash must track the latest applied migration.
        let stored_hash: Vec<u8> =
            get_migrations_value(&mut conn, DB_MIGRATION_HASH_FIELD).unwrap().unwrap();
        assert_eq!(&stored_hash[..], &*MIGRATION_HASHES[MIGRATION_HASHES.len() - 1]);

        let remaining: Vec<Vec<u8>> = conn
            .prepare("SELECT source FROM tags")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();

        assert!(!remaining.contains(&note_source_bytes(&output_only)));
        assert!(!remaining.contains(&note_source_bytes(&self_committed)));
        assert!(remaining.contains(&note_source_bytes(&self_directed)));
        assert!(remaining.contains(&note_source_bytes(&imported_input)));
        assert!(remaining.contains(&account_source));
        assert_eq!(remaining.len(), 3);
    }
}

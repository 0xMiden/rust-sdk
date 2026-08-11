use std::sync::LazyLock;

use rusqlite::{Connection, params};

use crate::db_management::errors::SqliteStoreError;
use crate::db_management::migration::SqliteMigration;
use crate::db_management::schema::SchemaHash;

// FIXTURE MIGRATIONS
// ================================================================================================

/// v1 stores assets and metadata in a single delimited column.
const FIXTURE_MIGRATION_V1: &str = r"
CREATE TABLE note_records (
    id TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
";

/// v2 splits the delimited column into separate assets and metadata columns.
const FIXTURE_MIGRATION_V2: &str = r"
CREATE TABLE note_records_new (
    id TEXT PRIMARY KEY,
    assets TEXT NOT NULL,
    metadata TEXT NOT NULL
);

INSERT INTO note_records_new (id, assets, metadata)
SELECT
    id,
    substr(value, 1, instr(value, '|') - 1),
    substr(value, instr(value, '|') + 1)
FROM note_records;

DROP TABLE note_records;
ALTER TABLE note_records_new RENAME TO note_records;
";

static FIXTURE_MIGRATION: LazyLock<SqliteMigration> =
    LazyLock::new(|| SqliteMigration::from_scripts(&[FIXTURE_MIGRATION_V1, FIXTURE_MIGRATION_V2]));

// HELPERS
// ================================================================================================

fn open_memory_db() -> Connection {
    Connection::open_in_memory().expect("in-memory database should open")
}

fn open_db_at_fixture_version(version: usize) -> Connection {
    let mut conn = open_memory_db();
    FIXTURE_MIGRATION
        .migrate_to_version(&mut conn, version)
        .expect("fixture migration should apply");
    conn
}

fn seed_fixture_v1(conn: &Connection) {
    conn.execute(
        "INSERT INTO note_records (id, value) VALUES (?1, ?2), (?3, ?4)",
        params!["note-a", "asset-a|meta-a", "note-b", "asset-b|meta-b"],
    )
    .expect("fixture rows should insert");
}

fn read_transformed_fixture_rows(conn: &Connection) -> Vec<(String, String, String)> {
    let mut stmt = conn
        .prepare("SELECT id, assets, metadata FROM note_records ORDER BY id")
        .expect("note_records should exist after migration");

    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("rows should query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should decode")
}

fn expected_transformed_rows() -> Vec<(String, String, String)> {
    vec![
        ("note-a".to_owned(), "asset-a".to_owned(), "meta-a".to_owned()),
        ("note-b".to_owned(), "asset-b".to_owned(), "meta-b".to_owned()),
    ]
}

// TESTS
// ================================================================================================

#[test]
fn schema_present_at_version_zero_fails() {
    let mut conn = open_memory_db();
    conn.execute_batch(FIXTURE_MIGRATION_V1)
        .expect("v1 schema should be created manually");

    assert!(
        !FIXTURE_MIGRATION.has_pending(&conn).expect("version should be readable"),
        "a database that records no version is not behind"
    );

    let err = FIXTURE_MIGRATION.apply(&mut conn).unwrap_err();
    assert!(matches!(err, SqliteStoreError::NotAClientStore));
}

#[test]
fn user_version_beyond_migrations_fails() {
    let latest = FIXTURE_MIGRATION.latest_version();
    let mut conn = open_db_at_fixture_version(latest);
    conn.pragma_update(None, "user_version", latest + 1)
        .expect("user_version should update");

    let err = FIXTURE_MIGRATION.apply(&mut conn).unwrap_err();
    let SqliteStoreError::SchemaTooNew { found, supported } = err else {
        panic!("a version beyond the migrations should be reported as too new, got {err:?}");
    };
    assert_eq!(found, latest + 1);
    assert_eq!(supported, latest);
}

#[test]
fn partial_migration_reopens_without_error() {
    let mut conn = open_db_at_fixture_version(1);
    seed_fixture_v1(&conn);

    FIXTURE_MIGRATION.apply(&mut conn).expect("partial database should upgrade");
    FIXTURE_MIGRATION.apply(&mut conn).expect("latest database should reopen");
}

#[test]
fn partial_migration_schema_drift_is_rejected() {
    let mut conn = open_db_at_fixture_version(1);
    conn.execute("ALTER TABLE note_records ADD COLUMN injected TEXT", [])
        .expect("manual schema change should apply");

    let err = FIXTURE_MIGRATION.apply(&mut conn).unwrap_err();
    let SqliteStoreError::SchemaDrift { version, expected, actual } = err else {
        panic!("a hand-modified schema should be reported as drift, got {err:?}");
    };
    assert_eq!(version, 1);
    assert_ne!(expected, actual);
}

#[test]
fn user_data_does_not_change_schema_hash() {
    let mut conn = open_memory_db();
    SqliteMigration::client()
        .apply(&mut conn)
        .expect("production schema should apply");

    let hash_before = SchemaHash::of(&conn).expect("schema hash should compute");
    assert_eq!(hash_before, SqliteMigration::client().expected_schema_hashes()[0]);

    conn.execute(
        "INSERT INTO settings (name, value) VALUES (?1, ?2)",
        params!["test-setting", b"value"],
    )
    .expect("user data should insert");

    let hash_after_data = SchemaHash::of(&conn).expect("schema hash should compute");
    assert_eq!(hash_before, hash_after_data);

    SqliteMigration::client()
        .apply(&mut conn)
        .expect("database with user data should reopen");
    assert_eq!(hash_before, SchemaHash::of(&conn).expect("schema hash should compute"));
}

#[test]
fn partial_migration_transforms_user_data() {
    let mut conn = open_db_at_fixture_version(1);
    seed_fixture_v1(&conn);

    FIXTURE_MIGRATION.apply(&mut conn).expect("partial database should upgrade");

    assert_eq!(read_transformed_fixture_rows(&conn), expected_transformed_rows());
}

#[test]
fn partial_migration_reapply_is_idempotent() {
    let mut conn = open_db_at_fixture_version(1);
    seed_fixture_v1(&conn);
    FIXTURE_MIGRATION.apply(&mut conn).expect("partial database should upgrade");

    let rows_before = read_transformed_fixture_rows(&conn);
    FIXTURE_MIGRATION.apply(&mut conn).expect("latest database should reopen");

    assert_eq!(read_transformed_fixture_rows(&conn), rows_before);
}

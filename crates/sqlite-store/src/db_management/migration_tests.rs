use std::sync::LazyLock;

use miden_client::store::SettingScope;
use rusqlite::{Connection, Transaction, params};
use rusqlite_migration::{HookError, HookResult};

use crate::db_management::errors::SqliteStoreError;
use crate::db_management::migration::{MigrationHook, SqliteMigration, SqliteMigrator};
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

/// v3 adds a column that a hook fills in.
const FIXTURE_MIGRATION_V3: &str = r"
ALTER TABLE note_records ADD COLUMN assets_reencoded TEXT NOT NULL DEFAULT '';
";

// FIXTURE SCHEMA HASHES
// ================================================================================================

/// The fingerprints the fixture versions build, pinned the way a shipped migration's is.
const FIXTURE_V1_HASH: &str = "0x1d5b0d9677b9365a008b14ef0aac7788eaa9d7cee1771c1f192adbfdcd1576e6";
const FIXTURE_V2_HASH: &str = "0xfa8b25c227e5af3f2f40ee7bf2c32954cf678a51bf4f644d58c67d92d23048f0";

/// v3 as its SQL alone builds it, which is also what a hook that only moves data leaves behind.
const FIXTURE_V3_HASH: &str = "0xc0c50bee012cd56746038d9fd44050c5d55dd60ca9b525131151b0c8e82c96b7";

/// v3 as [`index_reencoded_assets`] leaves it, carrying the index that hook creates.
const FIXTURE_V3_INDEXED_HASH: &str =
    "0xda8449bcda726cc7b50a7013139953ed5b6ba8e4bdd999f9bb0d19fd8bd2e244";

static FIXTURE_MIGRATION: LazyLock<SqliteMigrator> = LazyLock::new(|| {
    SqliteMigrator::new(&[
        SqliteMigration::new(FIXTURE_MIGRATION_V1, FIXTURE_V1_HASH),
        SqliteMigration::new(FIXTURE_MIGRATION_V2, FIXTURE_V2_HASH),
    ])
});

// FIXTURE HOOKS
// ================================================================================================

/// A transform `SQLite` has no expression for, standing in for re-encoding a serialized protocol
/// object.
fn reencode(assets: &str) -> String {
    assets.chars().rev().collect::<String>().to_uppercase()
}

/// Re-encodes every row through Rust, which is the reason a migration needs a hook at all.
fn reencode_assets(tx: &Transaction<'_>) -> HookResult {
    for (id, assets) in read_ids_and_assets(tx)? {
        tx.execute(
            "UPDATE note_records SET assets_reencoded = ?1 WHERE id = ?2",
            params![reencode(&assets), id],
        )?;
    }

    Ok(())
}

/// Fails on any row it cannot decode, which is how a hook reports data the new encoding does not
/// admit.
///
/// It finds no rows on an empty database, so building the schema from scratch still succeeds.
fn reencode_assets_as_amount(tx: &Transaction<'_>) -> HookResult {
    for (id, assets) in read_ids_and_assets(tx)? {
        let amount: u64 = assets.parse().map_err(|_| {
            HookError::Hook(format!("row {id} holds {assets}, which is not an amount"))
        })?;

        tx.execute(
            "UPDATE note_records SET assets_reencoded = ?1 WHERE id = ?2",
            params![amount, id],
        )?;
    }

    Ok(())
}

/// Creates schema of its own, so the version it belongs to builds more than its SQL describes.
fn index_reencoded_assets(tx: &Transaction<'_>) -> HookResult {
    tx.execute_batch(
        "CREATE INDEX idx_note_records_assets_reencoded ON note_records(assets_reencoded);",
    )?;

    Ok(())
}

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

/// The fixture migrations with a third version whose SQL adds a column, applied by `hook` when
/// there is one, and pinned to `expected_hash`.
fn fixture_migration_with_v3(
    hook: Option<MigrationHook>,
    expected_hash: &'static str,
) -> SqliteMigrator {
    let v3 = match hook {
        Some(hook) => SqliteMigration::with_hook(FIXTURE_MIGRATION_V3, hook, expected_hash),
        None => SqliteMigration::new(FIXTURE_MIGRATION_V3, expected_hash),
    };

    SqliteMigrator::new(&[
        SqliteMigration::new(FIXTURE_MIGRATION_V1, FIXTURE_V1_HASH),
        SqliteMigration::new(FIXTURE_MIGRATION_V2, FIXTURE_V2_HASH),
        v3,
    ])
}

fn seed_fixture_v1(conn: &Connection) {
    conn.execute(
        "INSERT INTO note_records (id, value) VALUES (?1, ?2), (?3, ?4)",
        params!["note-a", "asset-a|meta-a", "note-b", "asset-b|meta-b"],
    )
    .expect("fixture rows should insert");
}

fn read_ids_and_assets(conn: &Connection) -> Result<Vec<(String, String)>, rusqlite::Error> {
    let mut stmt = conn.prepare("SELECT id, assets FROM note_records ORDER BY id")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

    rows.collect()
}

fn read_fixture_v1_rows(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare("SELECT id, value FROM note_records ORDER BY id")
        .expect("the v1 table should exist");

    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("rows should query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should decode")
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

fn read_reencoded_assets(conn: &Connection) -> Vec<(String, String)> {
    let mut stmt = conn
        .prepare("SELECT id, assets_reencoded FROM note_records ORDER BY id")
        .expect("the hooked column should exist after migration");

    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
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

fn user_version(conn: &Connection) -> usize {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version should be readable")
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
    let client = SqliteMigrator::client();
    let mut conn = open_memory_db();
    client.apply(&mut conn).expect("production schema should apply");

    let hash_before = SchemaHash::of(&conn).expect("schema hash should compute");
    assert_eq!(hash_before.to_string(), client.expected_hash(client.latest_version()));

    conn.execute(
        "INSERT INTO settings (scope, name, value) VALUES (?1, ?2, ?3)",
        params![SettingScope::User.as_u8(), "test-setting", b"value"],
    )
    .expect("user data should insert");

    let hash_after_data = SchemaHash::of(&conn).expect("schema hash should compute");
    assert_eq!(hash_before, hash_after_data);

    client.apply(&mut conn).expect("database with user data should reopen");
    assert_eq!(hash_before, SchemaHash::of(&conn).expect("schema hash should compute"));
}

#[test]
fn client_store_at_version_one_upgrades_in_place() {
    let mut conn = open_memory_db();
    SqliteMigrator::client()
        .migrate_to_version(&mut conn, 1)
        .expect("version 1 of the production schema should apply");
    conn.execute(
        "INSERT INTO transactions (id, details, script_root, block_num, status_variant, status) \
         VALUES (?1, ?2, NULL, ?3, ?4, ?5)",
        params![b"transaction-id", b"details", 7, 0, b"status"],
    )
    .expect("a version 1 transaction should insert");

    SqliteMigrator::client()
        .apply(&mut conn)
        .expect("a version 1 store should upgrade");

    assert_eq!(user_version(&conn), SqliteMigrator::client().latest_version());
    let (id, status_variant): (Vec<u8>, u8) = conn
        .query_row("SELECT id, status_variant FROM transactions", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .expect("the transaction should have survived the upgrade");
    assert_eq!(id, b"transaction-id");
    assert_eq!(status_variant, 0);
    assert!(
        conn.prepare("SELECT block_num FROM transactions").is_err(),
        "the dropped column should be gone"
    );
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

#[test]
fn migration_hook_transforms_data_sql_cannot() {
    let mut conn = open_db_at_fixture_version(1);
    seed_fixture_v1(&conn);

    let migration = fixture_migration_with_v3(Some(reencode_assets), FIXTURE_V3_HASH);
    migration.apply(&mut conn).expect("hooked migration should apply");

    assert_eq!(user_version(&conn), migration.latest_version());
    assert_eq!(
        read_reencoded_assets(&conn),
        vec![
            ("note-a".to_owned(), reencode("asset-a")),
            ("note-b".to_owned(), reencode("asset-b")),
        ],
        "the hook should have rewritten every row"
    );
    assert_eq!(
        read_transformed_fixture_rows(&conn),
        expected_transformed_rows(),
        "the SQL of the versions before the hooked one should have run as well"
    );
}

#[test]
fn failing_migration_hook_rolls_back_the_upgrade() {
    let mut conn = open_db_at_fixture_version(1);
    seed_fixture_v1(&conn);
    let rows_before = read_fixture_v1_rows(&conn);

    let migration = fixture_migration_with_v3(Some(reencode_assets_as_amount), FIXTURE_V3_HASH);
    let err = migration.apply(&mut conn).unwrap_err();
    let SqliteStoreError::Migration(message) = err else {
        panic!("a hook that fails should be reported as a migration failure, got {err:?}");
    };
    assert!(
        message.contains("not an amount"),
        "the hook's own message should survive: {message}"
    );

    // Every pending migration and every hook share one transaction, so the failing hook takes the
    // SQL of the two versions before it down with it.
    assert_eq!(user_version(&conn), 1, "the version should not have advanced");
    assert_eq!(
        SchemaHash::of(&conn).expect("schema hash should compute").to_string(),
        migration.expected_hash(1),
        "the schema should still be the one version 1 builds"
    );
    assert_eq!(read_fixture_v1_rows(&conn), rows_before, "the rows should be untouched");
}

#[test]
fn schema_built_by_a_hook_is_covered_by_the_fingerprint() {
    let migration =
        fixture_migration_with_v3(Some(index_reencoded_assets), FIXTURE_V3_INDEXED_HASH);
    let without_hook = fixture_migration_with_v3(None, FIXTURE_V3_HASH);
    let latest = migration.latest_version();

    let mut hooked_db = open_memory_db();
    let mut plain_db = open_memory_db();
    without_hook
        .apply(&mut plain_db)
        .expect("the migration without a hook should apply");
    migration
        .apply(&mut hooked_db)
        .expect("the migration with the indexing hook should apply");

    assert_ne!(
        SchemaHash::of(&hooked_db).expect("schema hash should compute"),
        SchemaHash::of(&plain_db).expect("schema hash should compute"),
        "the index the hook creates should be part of the version's fingerprint"
    );

    let mut conn = open_db_at_fixture_version(1);
    seed_fixture_v1(&conn);
    migration.apply(&mut conn).expect("hooked migration should apply");

    conn.execute_batch("DROP INDEX idx_note_records_assets_reencoded")
        .expect("the index the hook created should be droppable");

    let err = migration.apply(&mut conn).unwrap_err();
    let SqliteStoreError::SchemaDrift { version, expected, actual } = err else {
        panic!("dropping what a hook built should be reported as drift, got {err:?}");
    };
    assert_eq!(version, latest);
    assert_ne!(expected, actual);
}

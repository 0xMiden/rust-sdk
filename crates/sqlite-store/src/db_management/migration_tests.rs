use std::sync::LazyLock;

use rusqlite::{Connection, params};
use rusqlite_migration::{M, Migrations};

// PRODUCTION MIGRATIONS
// ================================================================================================

static PRODUCTION_MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![M::up(include_str!("../store.sql")).foreign_key_check()])
});

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

static FIXTURE_MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![M::up(FIXTURE_MIGRATION_V1), M::up(FIXTURE_MIGRATION_V2)])
});

// HELPERS
// ================================================================================================

fn open_memory_db() -> Connection {
    Connection::open_in_memory().expect("in-memory database should open")
}

fn seed_fixture_v1(conn: &Connection) {
    conn.execute(
        "INSERT INTO note_records (id, value) VALUES (?1, ?2), (?3, ?4)",
        params!["note-a", "asset-a|meta-a", "note-b", "asset-b|meta-b"],
    )
    .expect("fixture rows should insert");
}

fn read_fixture_rows(conn: &Connection) -> Vec<(String, String, String)> {
    let mut stmt = conn
        .prepare("SELECT id, assets, metadata FROM note_records ORDER BY id")
        .expect("note_records should exist after migration");

    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("rows should query")
        .collect::<Result<Vec<_>, _>>()
        .expect("rows should decode")
}

// TESTS
// ================================================================================================

#[test]
fn production_migrations_validate() {
    PRODUCTION_MIGRATIONS
        .validate()
        .expect("store.sql should apply cleanly on a fresh database");
}

#[test]
fn migration_transforms_existing_rows() {
    let mut conn = open_memory_db();

    FIXTURE_MIGRATIONS.to_version(&mut conn, 1).expect("v1 migration should apply");
    seed_fixture_v1(&conn);

    FIXTURE_MIGRATIONS.to_latest(&mut conn).expect("v2 migration should apply");

    let rows = read_fixture_rows(&conn);
    assert_eq!(
        rows,
        vec![
            ("note-a".to_owned(), "asset-a".to_owned(), "meta-a".to_owned()),
            ("note-b".to_owned(), "asset-b".to_owned(), "meta-b".to_owned()),
        ]
    );
}

#[test]
fn migration_is_idempotent_on_reopen() {
    let mut conn = open_memory_db();

    FIXTURE_MIGRATIONS.to_version(&mut conn, 1).expect("v1 migration should apply");
    seed_fixture_v1(&conn);
    FIXTURE_MIGRATIONS.to_latest(&mut conn).expect("v2 migration should apply");

    let rows_before = read_fixture_rows(&conn);

    FIXTURE_MIGRATIONS
        .to_latest(&mut conn)
        .expect("re-applying latest migration should succeed");

    let rows_after = read_fixture_rows(&conn);
    assert_eq!(rows_before, rows_after);
}

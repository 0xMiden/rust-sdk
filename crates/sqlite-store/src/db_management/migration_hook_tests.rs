use std::sync::LazyLock;

use miden_client::account::AccountId;
use miden_client::note::NoteTag;
use miden_client::sync::{NoteTagRecord, NoteTagSource};
use miden_client::testing::common::ACCOUNT_ID_REGULAR;
use miden_client::utils::{Deserializable, Serializable};
use rusqlite::{Connection, Transaction, params};
use rusqlite_migration::{HookResult, M, Migrations};

use crate::SqliteStore;
use crate::db_management::utils::{apply_migrations_with, compute_expected_schema_hashes_for};

// FIXTURE MIGRATIONS
// ================================================================================================

/// v1 uses the production `tags` table layout.
const TAGS_V1_SCHEMA: &str = r"
CREATE TABLE tags (
    tag BLOB NOT NULL,
    source BLOB NOT NULL
);
CREATE UNIQUE INDEX idx_tags_tag_source ON tags(tag, source);
";

static TAGS_FIXTURE_MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![M::up(TAGS_V1_SCHEMA), M::up_with_hook("", migrate_legacy_tag_sources)])
});

const TAGS_FIXTURE_MIGRATION_COUNT: usize = 2;

static TAGS_FIXTURE_EXPECTED_SCHEMA_HASHES: LazyLock<
    Vec<miden_protocol::crypto::hash::blake::Blake3Digest<32>>,
> = LazyLock::new(|| {
    compute_expected_schema_hashes_for(&TAGS_FIXTURE_MIGRATIONS, TAGS_FIXTURE_MIGRATION_COUNT)
});

/// Rewrites legacy account-only `source` blobs into the current [`NoteTagSource`] wire format.
fn migrate_legacy_tag_sources(tx: &Transaction) -> HookResult {
    let mut stmt = tx.prepare("SELECT rowid, source FROM tags")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))?;

    for row in rows {
        let (rowid, source) = row?;
        if NoteTagSource::read_from_bytes(&source).is_ok() {
            continue;
        }

        let account_id = AccountId::read_from_bytes(&source).map_err(|err| {
            rusqlite_migration::HookError::Hook(format!(
                "legacy tag source is neither NoteTagSource nor AccountId: {err}"
            ))
        })?;
        let migrated_source = NoteTagSource::Account(account_id).to_bytes();
        tx.execute(
            "UPDATE tags SET source = ?1 WHERE rowid = ?2",
            params![migrated_source, rowid],
        )?;
    }

    Ok(())
}

// HELPERS
// ================================================================================================

fn open_db_at_tags_fixture_version(version: usize) -> Connection {
    let conn = Connection::open_in_memory().expect("in-memory database should open");
    let mut conn = conn;
    TAGS_FIXTURE_MIGRATIONS
        .to_version(&mut conn, version)
        .expect("fixture migration should apply");
    conn
}

fn seed_legacy_and_modern_tags(conn: &Connection) -> (NoteTagRecord, NoteTagRecord) {
    let account_id = AccountId::try_from(ACCOUNT_ID_REGULAR).expect("valid test account id");
    let account_tag = NoteTag::with_account_target(account_id);
    let legacy_account_record = NoteTagRecord::with_account_source(account_tag, account_id);
    let user_record = NoteTagRecord {
        tag: NoteTag::from(0xabcd_u32),
        source: NoteTagSource::User,
    };

    conn.execute(
        "INSERT INTO tags (tag, source) VALUES (?1, ?2), (?3, ?4)",
        params![
            legacy_account_record.tag.to_bytes(),
            account_id.to_bytes(),
            user_record.tag.to_bytes(),
            user_record.source.to_bytes(),
        ],
    )
    .expect("fixture tag rows should insert");

    (legacy_account_record, user_record)
}

fn apply_tags_fixture_migrations(
    conn: &mut Connection,
) -> Result<(), crate::db_management::errors::SqliteStoreError> {
    apply_migrations_with(conn, &TAGS_FIXTURE_MIGRATIONS, &TAGS_FIXTURE_EXPECTED_SCHEMA_HASHES)
}

// TESTS
// ================================================================================================

#[test]
fn partial_migration_transforms_tags_with_rust_hook() {
    let mut conn = open_db_at_tags_fixture_version(1);
    let (legacy_account_record, user_record) = seed_legacy_and_modern_tags(&conn);

    apply_tags_fixture_migrations(&mut conn).expect("partial database should upgrade");

    let stored_tags = SqliteStore::get_note_tags(&mut conn).expect("tags should deserialize");
    assert_eq!(stored_tags.len(), 2);
    assert!(stored_tags.contains(&legacy_account_record));
    assert!(stored_tags.contains(&user_record));

    apply_tags_fixture_migrations(&mut conn).expect("latest database should reopen");
    assert_eq!(
        SqliteStore::get_note_tags(&mut conn).expect("tags should deserialize"),
        stored_tags
    );
}

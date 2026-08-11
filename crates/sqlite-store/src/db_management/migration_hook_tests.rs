use std::sync::LazyLock;

use miden_client::account::AccountId;
use miden_client::note::NoteTag;
use miden_client::sync::{NoteTagRecord, NoteTagSource};
use miden_client::testing::common::ACCOUNT_ID_REGULAR;
use miden_client::utils::{Deserializable, Serializable};
use rusqlite::{Connection, Transaction, params};
use rusqlite_migration::{HookResult, M, Migrations, SchemaVersion};

use crate::SqliteStore;
use crate::db_management::errors::SqliteStoreError;
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

/// v2 transforms rows from a Rust hook rather than from SQL.
static TAGS_FIXTURE_MIGRATIONS: LazyLock<Migrations<'static>> = LazyLock::new(|| {
    Migrations::new(vec![M::up(TAGS_V1_SCHEMA), M::up_with_hook("", migrate_tag_sources)])
});

static TAGS_FIXTURE_EXPECTED_SCHEMA_HASHES: LazyLock<
    Vec<miden_protocol::crypto::hash::blake::Blake3Digest<32>>,
> = LazyLock::new(|| compute_expected_schema_hashes_for(&TAGS_FIXTURE_MIGRATIONS, 2));

/// Rewrites bare [`AccountId`] `source` blobs into the [`NoteTagSource`] wire format.
fn migrate_tag_sources(tx: &Transaction) -> HookResult {
    let mut stmt = tx.prepare("SELECT rowid, source FROM tags")?;
    let rows = stmt.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)))?;

    for row in rows {
        let (rowid, source) = row?;
        if NoteTagSource::read_from_bytes(&source).is_ok() {
            continue;
        }

        let account_id = AccountId::read_from_bytes(&source).map_err(|err| {
            rusqlite_migration::HookError::Hook(format!(
                "tag source is neither NoteTagSource nor AccountId: {err}"
            ))
        })?;
        tx.execute(
            "UPDATE tags SET source = ?1 WHERE rowid = ?2",
            params![NoteTagSource::Account(account_id).to_bytes(), rowid],
        )?;
    }

    Ok(())
}

// HELPERS
// ================================================================================================

fn open_tags_db_at_v1() -> Connection {
    let mut conn = Connection::open_in_memory().expect("in-memory database should open");
    TAGS_FIXTURE_MIGRATIONS
        .to_version(&mut conn, 1)
        .expect("fixture migration should apply");
    conn
}

fn apply_tags_fixture_migrations(conn: &mut Connection) -> Result<(), SqliteStoreError> {
    apply_migrations_with(conn, &TAGS_FIXTURE_MIGRATIONS, &TAGS_FIXTURE_EXPECTED_SCHEMA_HASHES)
}

fn test_account_id() -> AccountId {
    AccountId::try_from(ACCOUNT_ID_REGULAR).expect("valid test account id")
}

/// Returns the test account id with its first serialized byte replaced.
///
/// That byte is the most significant byte of the prefix felt, so overwriting it with a small value
/// leaves the version, type, and asset-callback bits in the prefix's least significant byte intact
/// and only lowers the felt.
fn account_id_starting_with(byte: u8) -> AccountId {
    let mut bytes = test_account_id().to_bytes();
    bytes[0] = byte;

    AccountId::read_from_bytes(&bytes).expect("account id with a patched prefix should stay valid")
}

fn insert_tag_row(conn: &Connection, tag: NoteTag, source: &[u8]) {
    conn.execute(
        "INSERT INTO tags (tag, source) VALUES (?1, ?2)",
        params![tag.to_bytes(), source],
    )
    .expect("fixture tag row should insert");
}

/// Counts rows whose `source` is still a bare serialized [`AccountId`].
///
/// Every [`NoteTagSource`] encoding is a discriminant byte plus a payload of a different total
/// length, so the blob length separates the two formats exactly.
fn untransformed_source_count(conn: &Connection) -> i64 {
    let bare_len =
        i64::try_from(AccountId::SERIALIZED_SIZE).expect("serialized size should fit in an i64");

    conn.query_row(
        "SELECT COUNT(*) FROM tags WHERE length(source) = ?1",
        params![bare_len],
        |row| row.get(0),
    )
    .expect("count should query")
}

// TESTS
// ================================================================================================

/// A hook must transform bare account sources while leaving rows already in the current format
/// alone, and reapplying it must be a no-op.
#[test]
fn hook_migration_transforms_tag_sources() {
    let mut conn = open_tags_db_at_v1();
    let account_id = test_account_id();
    let account_record =
        NoteTagRecord::with_account_source(NoteTag::with_account_target(account_id), account_id);
    let user_record = NoteTagRecord {
        tag: NoteTag::from(0xabcd_u32),
        source: NoteTagSource::User,
    };
    insert_tag_row(&conn, account_record.tag, &account_id.to_bytes());
    insert_tag_row(&conn, user_record.tag, &user_record.source.to_bytes());

    apply_tags_fixture_migrations(&mut conn).expect("partial database should upgrade");

    let stored_tags = SqliteStore::get_note_tags(&mut conn).expect("tags should deserialize");
    assert_eq!(stored_tags.len(), 2);
    assert!(stored_tags.contains(&account_record));
    assert!(stored_tags.contains(&user_record));

    apply_tags_fixture_migrations(&mut conn).expect("latest database should reopen");
    assert_eq!(
        SqliteStore::get_note_tags(&mut conn).expect("tags should deserialize"),
        stored_tags
    );
}

/// A bare account source must be recognized whatever its leading byte, including bytes that are
/// themselves [`NoteTagSource`] discriminants.
///
/// A bare account id starting with 2 is also a complete `NoteTagSource::User` encoding followed by
/// unread bytes, so a probe that ignores trailing bytes mistakes that row for a transformed one.
#[test]
fn hook_migration_catches_wrongly_converted_row() {
    let mut conn = open_tags_db_at_v1();
    let expected: Vec<NoteTagRecord> = (0..=4_u8)
        .map(|leading_byte| {
            let account_id = account_id_starting_with(leading_byte);
            // Tags are assigned independently of the account so no two rows collide on the
            // unique `(tag, source)` index.
            let tag = NoteTag::from(u32::from(leading_byte));
            insert_tag_row(&conn, tag, &account_id.to_bytes());

            NoteTagRecord::with_account_source(tag, account_id)
        })
        .collect();

    apply_tags_fixture_migrations(&mut conn).expect("partial database should upgrade");

    assert_eq!(untransformed_source_count(&conn), 0);
    let stored_tags = SqliteStore::get_note_tags(&mut conn).expect("tags should deserialize");
    for record in expected {
        assert!(stored_tags.contains(&record), "row {:?} was not transformed", record.tag);
    }
}

/// A failing hook must leave the database at its previous version with no rows rewritten.
#[test]
fn hook_migration_failure_rolls_back() {
    let mut conn = open_tags_db_at_v1();
    let account_id = test_account_id();
    insert_tag_row(&conn, NoteTag::with_account_target(account_id), &account_id.to_bytes());
    // Too short to decode as either a NoteTagSource or an AccountId.
    insert_tag_row(&conn, NoteTag::from(0x1234_u32), &[0xff; 8]);

    let err = apply_tags_fixture_migrations(&mut conn).unwrap_err();
    assert!(matches!(err, SqliteStoreError::MigrationError(_)));

    let version = TAGS_FIXTURE_MIGRATIONS
        .current_version(&conn)
        .expect("version should be readable");
    assert!(matches!(version, SchemaVersion::Inside(ver) if ver.get() == 1));

    // The well-formed row precedes the undecodable one in rowid order, so its rewrite must have
    // been rolled back with the rest of the migration.
    assert_eq!(untransformed_source_count(&conn), 1);
}

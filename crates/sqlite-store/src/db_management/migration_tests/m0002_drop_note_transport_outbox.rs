//! Tests for migration `0002`, which deletes the client scope `note_transport_outbox` setting.

use rusqlite::params;

use super::{CLIENT_SCOPE, USER_SCOPE, open_memory_db};
use crate::db_management::migration::SqliteMigrator;

#[test]
fn drops_only_the_client_outbox_row() {
    let mut conn = open_memory_db();
    SqliteMigrator::client()
        .migrate_to_version(&mut conn, 1)
        .expect("version 1 of the production schema should apply");
    let insert_setting = |scope: u8, name: &str| {
        conn.execute(
            "INSERT INTO settings (scope, name, value) VALUES (?1, ?2, ?3)",
            params![scope, name, b"value"],
        )
        .expect("a setting should insert");
    };
    insert_setting(CLIENT_SCOPE, "note_transport_outbox");
    insert_setting(CLIENT_SCOPE, "note_transport_cursor");
    insert_setting(USER_SCOPE, "note_transport_outbox");

    SqliteMigrator::client()
        .migrate_to_version(&mut conn, 2)
        .expect("a version 1 store should upgrade to version 2");

    let mut stmt = conn
        .prepare("SELECT scope, name FROM settings ORDER BY scope, name")
        .expect("settings should be readable");
    let settings: Vec<(u8, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("settings should be readable")
        .collect::<Result<_, _>>()
        .expect("settings should decode");
    assert_eq!(
        settings,
        [
            (CLIENT_SCOPE, "note_transport_cursor".to_owned()),
            (USER_SCOPE, "note_transport_outbox".to_owned()),
        ],
        "only the client outbox row should be gone"
    );
}

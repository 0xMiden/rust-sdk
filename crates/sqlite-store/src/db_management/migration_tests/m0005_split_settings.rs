//! Tests for migration `0005`, which splits the `settings` table into `client_config` and
//! `user_settings`.

use rusqlite::{Connection, params};

use super::{CLIENT_SCOPE, USER_SCOPE, open_memory_db};
use crate::db_management::migration::SqliteMigrator;

fn names(conn: &Connection, table: &str) -> Vec<String> {
    conn.prepare(&format!("SELECT name FROM {table} ORDER BY name"))
        .expect("the table should be readable")
        .query_map([], |row| row.get(0))
        .expect("the table should be readable")
        .collect::<Result<_, _>>()
        .expect("the names should decode")
}

#[test]
fn keeps_user_rows_and_protocol_configs() {
    let mut conn = open_memory_db();
    SqliteMigrator::client()
        .migrate_to_version(&mut conn, 4)
        .expect("version 4 of the production schema should apply");
    for (scope, name) in [
        (CLIENT_SCOPE, "protocol_config:0x01"),
        (CLIENT_SCOPE, "rpc_limits"),
        (CLIENT_SCOPE, "note_transport_cursors"),
        (USER_SCOPE, "rpc_limits"),
    ] {
        conn.execute(
            "INSERT INTO settings (scope, name, value) VALUES (?1, ?2, ?3)",
            params![scope, name, b"value"],
        )
        .expect("a setting should insert");
    }

    SqliteMigrator::client()
        .apply(&mut conn)
        .expect("a version 4 store should upgrade");

    assert_eq!(names(&conn, "client_config"), ["protocol_config:0x01"]);
    assert_eq!(names(&conn, "user_settings"), ["rpc_limits"]);
    let settings_tables: i64 = conn
        .query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'settings'", [], |row| {
            row.get(0)
        })
        .expect("the schema should be readable");
    assert_eq!(settings_tables, 0, "the settings table should be gone");
}

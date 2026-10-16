//! Tests for migration `0002`, which deletes the client `note_transport_outbox` row.

use rusqlite::params;

use super::open_memory_db;
use crate::db_management::migration::SqliteMigrator;

#[test]
fn drops_only_the_client_outbox_row() {
    let mut conn = open_memory_db();
    SqliteMigrator::client()
        .migrate_to_version(&mut conn, 1)
        .expect("version 1 of the production schema should apply");
    let insert_row = |table: &str, name: &str| {
        conn.execute(
            &format!("INSERT INTO {table} (name, value) VALUES (?1, ?2)"),
            params![name, b"value"],
        )
        .expect("a row should insert");
    };
    insert_row("client_config", "note_transport_outbox");
    insert_row("client_config", "note_transport_cursor");
    insert_row("user_settings", "note_transport_outbox");

    SqliteMigrator::client()
        .apply(&mut conn)
        .expect("a version 1 store should upgrade");

    let mut stmt = conn
        .prepare(
            "SELECT 'client_config', name FROM client_config \
             UNION ALL SELECT 'user_settings', name FROM user_settings ORDER BY 1, 2",
        )
        .expect("the rows should be readable");
    let rows: Vec<(String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("the rows should be readable")
        .collect::<Result<_, _>>()
        .expect("the rows should decode");
    assert_eq!(
        rows,
        [
            ("client_config".to_owned(), "note_transport_cursor".to_owned()),
            ("user_settings".to_owned(), "note_transport_outbox".to_owned()),
        ],
        "only the client outbox row should be gone"
    );
}

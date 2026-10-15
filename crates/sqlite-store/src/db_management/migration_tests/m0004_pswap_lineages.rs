//! Tests for migration `0004`, which moves the PSWAP lineages to the `pswap_lineages` table.

use rusqlite::params;

use super::{CLIENT_SCOPE, USER_SCOPE, open_memory_db};
use crate::db_management::migration::SqliteMigrator;

#[test]
fn drops_only_the_client_pswap_rows() {
    let mut conn = open_memory_db();
    SqliteMigrator::client()
        .migrate_to_version(&mut conn, 3)
        .expect("version 3 of the production schema should apply");
    let insert_setting = |scope: u8, name: &str| {
        conn.execute(
            "INSERT INTO settings (scope, name, value) VALUES (?1, ?2, ?3)",
            params![scope, name, b"value"],
        )
        .expect("a setting should insert");
    };
    insert_setting(CLIENT_SCOPE, "pswap/order/0100000000000000");
    insert_setting(CLIENT_SCOPE, "pswap/tip/0x0100000000000000");
    insert_setting(CLIENT_SCOPE, "rpc_limits");
    insert_setting(USER_SCOPE, "pswap/order/0100000000000000");

    SqliteMigrator::client()
        .migrate_to_version(&mut conn, 4)
        .expect("a version 3 store should upgrade to version 4");

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
            (CLIENT_SCOPE, "rpc_limits".to_owned()),
            (USER_SCOPE, "pswap/order/0100000000000000".to_owned()),
        ],
        "only the client PSWAP rows should be gone"
    );

    let lineages: i64 = conn
        .query_row("SELECT COUNT(*) FROM pswap_lineages", [], |row| row.get(0))
        .expect("the lineage table should exist");
    assert_eq!(lineages, 0, "the migration should not convert the old rows");
}

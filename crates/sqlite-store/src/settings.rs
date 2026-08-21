//! Settings-related database operations.

use std::string::String;
use std::vec::Vec;

use miden_client::store::StoreError;
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, ToSql, params};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

impl SqliteStore {
    pub(crate) fn get_setting<T: FromSql>(
        conn: &mut Connection,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        conn.transaction()
            .into_store_error()?
            .query_row("SELECT value FROM settings WHERE name = $1", params![name], |row| {
                row.get(0)
            })
            .optional()
            .into_store_error()
    }

    pub(crate) fn set_setting<T: ToSql>(
        conn: &Connection,
        name: &str,
        value: &T,
    ) -> rusqlite::Result<()> {
        let count =
            conn.execute(insert_sql!(settings { name, value } | REPLACE), params![name, value])?;

        debug_assert_eq!(count, 1);

        Ok(())
    }

    /// Returns `true` if a row was deleted, `false` if `name` wasn't present.
    pub(crate) fn remove_setting(conn: &Connection, name: &str) -> Result<bool, StoreError> {
        let count = conn
            .execute("DELETE FROM settings WHERE name = $1", params![name])
            .into_store_error()?;

        Ok(count > 0)
    }

    pub(crate) fn list_setting_keys(conn: &Connection) -> Result<Vec<String>, StoreError> {
        let mut stmt = conn.prepare("SELECT name FROM settings").into_store_error()?;
        stmt.query_map([], |row| row.get::<_, String>(0))
            .into_store_error()?
            .collect::<Result<Vec<String>, _>>()
            .into_store_error()
    }
}

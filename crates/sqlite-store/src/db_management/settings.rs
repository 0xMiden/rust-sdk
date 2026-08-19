//! Persistence for the client's key-value settings table.

use std::string::String;
use std::vec::Vec;

use miden_client::store::{SettingMutation, StoreError};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, Result, ToSql, params};

use crate::sql_error::SqlResultExt;
use crate::{SqliteStore, insert_sql, subst, with_write_tx};

impl SqliteStore {
    /// Applies the provided setting mutations atomically.
    pub(crate) fn apply_settings_mutations(
        conn: &mut Connection,
        mutations: &[SettingMutation],
    ) -> Result<(), StoreError> {
        with_write_tx(conn, |tx| {
            for mutation in mutations {
                match mutation {
                    SettingMutation::Set { key, value } => set_setting(tx, key, value)?,
                    SettingMutation::Remove { key } => remove_setting(tx, key)?,
                }
            }
            Ok(())
        })
    }
}

pub fn get_setting<T: FromSql>(conn: &Connection, name: &str) -> Result<Option<T>, StoreError> {
    conn.query_row("SELECT value FROM settings WHERE name = $1", params![name], |row| row.get(0))
        .optional()
        .into_store_error()
}

pub fn set_setting<T: ToSql>(conn: &Connection, name: &str, value: &T) -> Result<(), StoreError> {
    conn.execute(insert_sql!(settings { name, value } | REPLACE), params![name, value])
        .into_store_error()?;

    Ok(())
}

pub fn remove_setting(conn: &Connection, name: &str) -> Result<(), StoreError> {
    conn.execute("DELETE FROM settings WHERE name = $1", params![name])
        .into_store_error()?;

    Ok(())
}

pub fn list_setting_keys(conn: &Connection) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare("SELECT name FROM settings").into_store_error()?;
    stmt.query_map([], |row| row.get::<_, String>(0))
        .into_store_error()?
        .collect::<Result<Vec<String>, _>>()
        .into_store_error()
}

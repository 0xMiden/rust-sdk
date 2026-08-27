//! Settings-related database operations.

use std::string::String;
use std::vec::Vec;

use miden_client::store::{SettingDomain, SettingScope, StoreError};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, ToSql, params};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

impl SqliteStore {
    pub(crate) fn get_setting<T: FromSql>(
        conn: &mut Connection,
        domain: &SettingDomain,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        conn.transaction()
            .into_store_error()?
            .query_row(
                "SELECT value FROM settings WHERE scope = $1 AND domain = $2 AND name = $3",
                params![domain.scope().as_str(), domain.name(), name],
                |row| row.get(0),
            )
            .optional()
            .into_store_error()
    }

    pub(crate) fn set_setting<T: ToSql>(
        conn: &Connection,
        domain: &SettingDomain,
        name: &str,
        value: &T,
    ) -> rusqlite::Result<()> {
        let count = conn.execute(
            insert_sql!(settings { scope, domain, name, value } | REPLACE),
            params![domain.scope().as_str(), domain.name(), name, value],
        )?;

        debug_assert_eq!(count, 1);

        Ok(())
    }

    /// Returns `true` if a row was deleted, `false` if `name` wasn't present.
    pub(crate) fn remove_setting(
        conn: &Connection,
        domain: &SettingDomain,
        name: &str,
    ) -> Result<bool, StoreError> {
        let count = conn
            .execute(
                "DELETE FROM settings WHERE scope = $1 AND domain = $2 AND name = $3",
                params![domain.scope().as_str(), domain.name(), name],
            )
            .into_store_error()?;

        Ok(count > 0)
    }

    pub(crate) fn list_setting_keys(
        conn: &Connection,
        domain: &SettingDomain,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt = conn
            .prepare("SELECT name FROM settings WHERE scope = $1 AND domain = $2")
            .into_store_error()?;

        stmt.query_map(params![domain.scope().as_str(), domain.name()], |row| {
            row.get::<_, String>(0)
        })
        .into_store_error()?
        .collect::<Result<Vec<String>, _>>()
        .into_store_error()
    }

    pub(crate) fn list_user_setting_domains(conn: &Connection) -> Result<Vec<String>, StoreError> {
        let mut stmt = conn
            .prepare("SELECT DISTINCT domain FROM settings WHERE scope = $1")
            .into_store_error()?;

        stmt.query_map(params![SettingScope::User.as_str()], |row| row.get::<_, String>(0))
            .into_store_error()?
            .collect::<Result<Vec<String>, _>>()
            .into_store_error()
    }
}

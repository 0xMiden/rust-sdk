//! Settings-related database operations.

use std::string::String;
use std::vec::Vec;

use miden_client::store::{SettingScope, StoreError};
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, ToSql, params};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

impl SqliteStore {
    pub(crate) fn get_setting<T: FromSql>(
        conn: &mut Connection,
        scope: SettingScope,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        conn.transaction()
            .into_store_error()?
            .query_row(
                "SELECT value FROM settings WHERE scope = $1 AND name = $2",
                params![scope.as_u8(), name],
                |row| row.get(0),
            )
            .optional()
            .into_store_error()
    }

    pub(crate) fn set_setting<T: ToSql>(
        conn: &Connection,
        scope: SettingScope,
        name: &str,
        value: &T,
    ) -> rusqlite::Result<()> {
        let count = conn.execute(
            insert_sql!(settings { scope, name, value } | REPLACE),
            params![scope.as_u8(), name, value],
        )?;

        debug_assert_eq!(count, 1);

        Ok(())
    }

    /// Returns `true` if a row was deleted, `false` if `name` wasn't present.
    pub(crate) fn remove_setting(
        conn: &Connection,
        scope: SettingScope,
        name: &str,
    ) -> Result<bool, StoreError> {
        let count = conn
            .execute(
                "DELETE FROM settings WHERE scope = $1 AND name = $2",
                params![scope.as_u8(), name],
            )
            .into_store_error()?;

        Ok(count > 0)
    }

    pub(crate) fn list_setting_keys(
        conn: &Connection,
        scope: SettingScope,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt =
            conn.prepare("SELECT name FROM settings WHERE scope = $1").into_store_error()?;

        stmt.query_map(params![scope.as_u8()], |row| row.get::<_, String>(0))
            .into_store_error()?
            .collect::<Result<Vec<String>, _>>()
            .into_store_error()
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::store::{SettingScope, Store};

    use super::SqliteStore;
    use crate::sql_error::SqlResultExt;
    use crate::tests::create_test_store;

    const KEY: &str = "a-key";

    /// Writes a client-scoped row the way the client would, which the user scope must not reach.
    async fn write_client_row(store: &SqliteStore, value: &[u8]) {
        let value = value.to_vec();
        store
            .interact_with_connection(move |conn| {
                SqliteStore::set_setting(conn, SettingScope::Client, KEY, &value).into_store_error()
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn set_get_remove_round_trip() {
        let store = create_test_store().await;

        store
            .set_setting(SettingScope::User, KEY.into(), b"value".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.get_setting(SettingScope::User, KEY.into()).await.unwrap(),
            Some(b"value".to_vec())
        );

        assert!(store.remove_setting(SettingScope::User, KEY.into()).await.unwrap());
        assert_eq!(store.get_setting(SettingScope::User, KEY.into()).await.unwrap(), None);
        assert!(!store.remove_setting(SettingScope::User, KEY.into()).await.unwrap());
    }

    /// The same key name in both scopes addresses two different rows, so a user can neither read
    /// nor overwrite the client's.
    #[tokio::test]
    async fn a_client_row_is_out_of_reach_of_the_user_scope() {
        let store = create_test_store().await;
        write_client_row(&store, b"client").await;

        assert_eq!(store.get_setting(SettingScope::User, KEY.into()).await.unwrap(), None);

        store
            .set_setting(SettingScope::User, KEY.into(), b"user".to_vec())
            .await
            .unwrap();
        assert_eq!(
            store.get_setting(SettingScope::Client, KEY.into()).await.unwrap(),
            Some(b"client".to_vec())
        );

        assert!(store.remove_setting(SettingScope::User, KEY.into()).await.unwrap());
        assert_eq!(
            store.get_setting(SettingScope::Client, KEY.into()).await.unwrap(),
            Some(b"client".to_vec())
        );
    }

    #[tokio::test]
    async fn listing_keys_excludes_the_other_scope() {
        let store = create_test_store().await;
        write_client_row(&store, b"client").await;

        store
            .set_setting(SettingScope::User, "mine".into(), b"u".to_vec())
            .await
            .unwrap();

        assert_eq!(store.list_setting_keys(SettingScope::User).await.unwrap(), vec!["mine"]);
        assert_eq!(store.list_setting_keys(SettingScope::Client).await.unwrap(), vec![KEY]);
    }
}

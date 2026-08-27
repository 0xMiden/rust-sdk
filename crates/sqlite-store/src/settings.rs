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

#[cfg(test)]
mod tests {
    use miden_client::store::{SettingDomain, Store};
    use rusqlite::{OptionalExtension, params};

    use super::SqliteStore;
    use crate::sql_error::SqlResultExt;
    use crate::tests::create_test_store;

    const KEY: &str = "a-key";

    /// Writes a client-scoped row the way the client would. Client domains cannot be built from
    /// this crate, which is the property the tests below rely on.
    async fn write_client_row(store: &SqliteStore, domain: &str, value: &[u8]) {
        let (domain, value) = (domain.to_string(), value.to_vec());
        store
            .interact_with_connection(move |conn| {
                conn.execute(
                    "INSERT INTO settings (scope, domain, name, value) VALUES ('client', ?1, ?2, ?3)",
                    params![domain, KEY, value],
                )
                .into_store_error()?;
                Ok(())
            })
            .await
            .unwrap();
    }

    /// Reads back a client-scoped row, so a test can assert the user never disturbed it.
    async fn read_client_row(store: &SqliteStore, domain: &str) -> Option<Vec<u8>> {
        let domain = domain.to_string();
        store
            .interact_with_connection(move |conn| {
                conn.query_row(
                    "SELECT value FROM settings WHERE scope = 'client' AND domain = ?1 AND name = ?2",
                    params![domain, KEY],
                    |row| row.get(0),
                )
                .optional()
                .into_store_error()
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn set_get_remove_round_trip() {
        let store = create_test_store().await;
        let domain = SettingDomain::new("app").unwrap();

        assert_eq!(store.get_setting(&domain, KEY.into()).await.unwrap(), None);

        store.set_setting(&domain, KEY.into(), b"value".to_vec()).await.unwrap();
        assert_eq!(store.get_setting(&domain, KEY.into()).await.unwrap(), Some(b"value".to_vec()));

        // Writing the same key again replaces the value rather than being ignored.
        store.set_setting(&domain, KEY.into(), b"newer".to_vec()).await.unwrap();
        assert_eq!(store.get_setting(&domain, KEY.into()).await.unwrap(), Some(b"newer".to_vec()));

        assert!(store.remove_setting(&domain, KEY.into()).await.unwrap());
        assert!(!store.remove_setting(&domain, KEY.into()).await.unwrap());
        assert_eq!(store.get_setting(&domain, KEY.into()).await.unwrap(), None);
    }

    /// A user domain may reuse a client domain's name. The scope keeps the two apart, so the user
    /// can neither read the client's row nor overwrite it.
    #[tokio::test]
    async fn a_client_row_is_out_of_reach_of_a_domain_with_the_same_name() {
        let store = create_test_store().await;
        write_client_row(&store, "rpc", b"client").await;

        let user_domain = SettingDomain::new("rpc").unwrap();
        assert_eq!(store.get_setting(&user_domain, KEY.into()).await.unwrap(), None);

        store.set_setting(&user_domain, KEY.into(), b"user".to_vec()).await.unwrap();
        assert_eq!(
            store.get_setting(&user_domain, KEY.into()).await.unwrap(),
            Some(b"user".to_vec())
        );
        assert_eq!(read_client_row(&store, "rpc").await, Some(b"client".to_vec()));

        // Nor can it delete it: the removal only reaches the user's own row.
        assert!(store.remove_setting(&user_domain, KEY.into()).await.unwrap());
        assert_eq!(read_client_row(&store, "rpc").await, Some(b"client".to_vec()));
    }

    /// The listing is filtered by scope as well as by domain, so a client row sitting in a
    /// same-named domain does not leak into it.
    #[tokio::test]
    async fn listing_keys_is_scoped_to_one_domain() {
        let store = create_test_store().await;
        let one = SettingDomain::new("one").unwrap();
        let other = SettingDomain::new("other").unwrap();

        store.set_setting(&one, "one-key".into(), b"1".to_vec()).await.unwrap();
        store.set_setting(&other, "other-key".into(), b"2".to_vec()).await.unwrap();
        write_client_row(&store, "one", b"client").await;

        assert_eq!(store.list_setting_keys(&one).await.unwrap(), vec!["one-key".to_string()]);
        assert_eq!(store.list_setting_keys(&other).await.unwrap(), vec!["other-key".to_string()]);
    }

    /// The domain listing a user sees never names a client domain.
    #[tokio::test]
    async fn listing_domains_excludes_client_domains() {
        let store = create_test_store().await;
        write_client_row(&store, "rpc", b"c").await;

        store
            .set_setting(&SettingDomain::new("app").unwrap(), KEY.into(), b"u".to_vec())
            .await
            .unwrap();

        assert_eq!(store.list_user_setting_domains().await.unwrap(), vec!["app".to_string()]);
    }
}

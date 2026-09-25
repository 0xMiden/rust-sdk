//! Settings-related database operations.

use std::string::String;
use std::vec::Vec;

use miden_client::note::NoteId;
use miden_client::pswap::{
    PSWAP_ORDER_SETTING_PREFIX,
    PswapLineageRecord,
    pswap_order_setting_key,
    pswap_tip_setting_key,
};
use miden_client::store::proto::{self, ProtobufValue};
use miden_client::store::{SettingMutation, SettingScope, Store, StoreError};
use miden_client::utils::Serializable;
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, ToSql, params};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

impl SqliteStore {
    pub(crate) fn get_setting<T: FromSql>(
        conn: &Connection,
        scope: SettingScope,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        conn.query_row(
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
    ) -> Result<(), StoreError> {
        let count = conn
            .execute(
                insert_sql!(settings { scope, name, value } | REPLACE),
                params![scope.as_u8(), name, value],
            )
            .into_store_error()?;

        if count != 1 {
            return Err(StoreError::DatabaseError(format!(
                "writing setting {name:?} in scope {scope:?} affected {count} rows, expected 1"
            )));
        }

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

        if count > 1 {
            return Err(StoreError::DatabaseError(format!(
                "removing setting {name:?} in scope {scope:?} affected {count} rows, expected at \
                 most 1"
            )));
        }

        Ok(count == 1)
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

// CLIENT VALUES
// ================================================================================================

// The client values in `settings` keep the client's keys, and this store writes them as protobuf
// messages.
impl SqliteStore {
    pub(crate) async fn get_proto_setting<T: ProtobufValue>(
        &self,
        key: String,
    ) -> Result<Option<T>, StoreError> {
        let Some(bytes) = Store::get_setting(self, SettingScope::Client, key).await? else {
            return Ok(None);
        };
        Ok(Some(proto::decode(&bytes)?))
    }

    pub(crate) async fn set_proto_setting<T: ProtobufValue>(
        &self,
        key: String,
        value: &T,
    ) -> Result<(), StoreError> {
        Store::set_setting(self, SettingScope::Client, key, proto::encode(value)).await
    }

    /// Writes `value`, or removes the entry when `is_empty` holds, so empty collections leave no
    /// row behind.
    pub(crate) async fn set_proto_setting_or_remove<T: ProtobufValue>(
        &self,
        key: String,
        value: &T,
        is_empty: bool,
    ) -> Result<(), StoreError> {
        if is_empty {
            Store::remove_setting(self, SettingScope::Client, key).await?;
            return Ok(());
        }
        self.set_proto_setting(key, value).await
    }

    pub(crate) async fn put_pswap_lineage(
        &self,
        record: &PswapLineageRecord,
        old_tip: Option<NoteId>,
        new_tip: Option<NoteId>,
    ) -> Result<(), StoreError> {
        let mut mutations = vec![SettingMutation::Set {
            key: pswap_order_setting_key(record.order_id()),
            value: proto::encode(record),
        }];
        if let Some(old_tip) = old_tip {
            mutations.push(SettingMutation::Remove { key: pswap_tip_setting_key(old_tip) });
        }
        if let Some(new_tip) = new_tip {
            mutations.push(SettingMutation::Set {
                key: pswap_tip_setting_key(new_tip),
                value: record.order_id().to_bytes(),
            });
        }
        Store::apply_settings_mutations(self, SettingScope::Client, mutations).await
    }

    pub(crate) async fn list_pswap_lineages(&self) -> Result<Vec<PswapLineageRecord>, StoreError> {
        let mut records = Vec::new();
        for key in Store::list_setting_keys(self, SettingScope::Client).await? {
            if key.starts_with(PSWAP_ORDER_SETTING_PREFIX)
                && let Some(record) = self.get_proto_setting(key).await?
            {
                records.push(record);
            }
        }
        Ok(records)
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client::store::{SettingScope, Store};

    use super::SqliteStore;
    use crate::tests::create_test_store;

    const KEY: &str = "a-key";

    /// Writes a client-scoped row the way the client would, which the user scope must not reach.
    async fn write_client_row(store: &SqliteStore, value: &[u8]) {
        let value = value.to_vec();
        store
            .interact_with_connection(move |conn| {
                SqliteStore::set_setting(conn, SettingScope::Client, KEY, &value)
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

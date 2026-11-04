//! Settings-related database operations.
//!
//! The store keeps two key-value tables. `user_settings` holds the values of the user of the
//! client. `client_config` holds the data of the client as protobuf messages.

use std::string::String;
use std::vec::Vec;

use miden_client_core::errors::StoreError;
use miden_client_proto::{self as proto, ProtobufValue};
use miden_protocol::Word;
use rusqlite::{Connection, OptionalExtension, params};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;
use crate::{insert_sql, subst};

// CLIENT CONFIG KEYS
// ================================================================================================

pub(crate) const RPC_LIMITS_KEY: &str = "rpc_limits";
pub(crate) const TRANSACTION_ENCRYPTION_KEY_KEY: &str = "transaction_encryption_key";
pub(crate) const NOTE_TRANSPORT_SYNC_STATE_KEY: &str = "note_transport_sync_state";

/// Returns the `client_config` key of the protocol configuration for `commitment`.
pub(crate) fn protocol_config_key(commitment: Word) -> String {
    format!("protocol_config:{commitment}")
}

// KEY-VALUE TABLES
// ================================================================================================

impl SqliteStore {
    // USER SETTINGS
    // --------------------------------------------------------------------------------------------

    pub(crate) fn get_user_setting(
        conn: &Connection,
        name: &str,
    ) -> Result<Option<Vec<u8>>, StoreError> {
        conn.query_row("SELECT value FROM user_settings WHERE name = $1", params![name], |row| {
            row.get(0)
        })
        .optional()
        .into_store_error()
    }

    pub(crate) fn set_user_setting(
        conn: &Connection,
        name: &str,
        value: &[u8],
    ) -> Result<(), StoreError> {
        let count = conn
            .execute(insert_sql!(user_settings { name, value } | REPLACE), params![name, value])
            .into_store_error()?;

        if count != 1 {
            return Err(StoreError::DatabaseError(format!(
                "writing user setting {name:?} affected {count} rows, expected 1"
            )));
        }

        Ok(())
    }

    /// Returns `true` if a row was deleted, `false` if `name` wasn't present.
    pub(crate) fn remove_user_setting(conn: &Connection, name: &str) -> Result<bool, StoreError> {
        let count = conn
            .execute("DELETE FROM user_settings WHERE name = $1", params![name])
            .into_store_error()?;

        if count > 1 {
            return Err(StoreError::DatabaseError(format!(
                "removing user setting {name:?} affected {count} rows, expected at most 1"
            )));
        }

        Ok(count == 1)
    }

    pub(crate) fn list_user_setting_keys(conn: &Connection) -> Result<Vec<String>, StoreError> {
        let mut stmt = conn.prepare("SELECT name FROM user_settings").into_store_error()?;

        stmt.query_map([], |row| row.get::<_, String>(0))
            .into_store_error()?
            .collect::<Result<Vec<String>, _>>()
            .into_store_error()
    }

    // CLIENT CONFIG
    // --------------------------------------------------------------------------------------------

    /// Reads a client config value that this store wrote as a protobuf message.
    pub(crate) fn get_client_config<T: ProtobufValue>(
        conn: &Connection,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        let Some(bytes) = conn
            .query_row("SELECT value FROM client_config WHERE name = $1", params![name], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .optional()
            .into_store_error()?
        else {
            return Ok(None);
        };
        Ok(Some(proto::decode_unchecked(&bytes)?))
    }

    /// Writes a client config value as a protobuf message.
    pub(crate) fn set_client_config<T: ProtobufValue>(
        conn: &Connection,
        name: &str,
        value: &T,
    ) -> Result<(), StoreError> {
        conn.execute(
            insert_sql!(client_config { name, value } | REPLACE),
            params![name, proto::encode(value)],
        )
        .into_store_error()?;
        Ok(())
    }

    pub(crate) fn remove_client_config(conn: &Connection, name: &str) -> Result<(), StoreError> {
        conn.execute("DELETE FROM client_config WHERE name = $1", params![name])
            .into_store_error()?;
        Ok(())
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client_core::store::Store;

    use crate::tests::create_test_store;

    const KEY: &str = "a-key";

    #[tokio::test]
    async fn set_get_remove_round_trip() {
        let store = create_test_store().await;

        store.set_setting(KEY.into(), b"value".to_vec()).await.unwrap();
        assert_eq!(store.get_setting(KEY.into()).await.unwrap(), Some(b"value".to_vec()));
        assert_eq!(store.list_setting_keys().await.unwrap(), vec![KEY]);

        assert!(store.remove_setting(KEY.into()).await.unwrap());
        assert_eq!(store.get_setting(KEY.into()).await.unwrap(), None);
        assert!(!store.remove_setting(KEY.into()).await.unwrap());
    }
}

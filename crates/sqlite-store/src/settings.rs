//! Settings-related database operations.
//!
//! The store keeps two key-value tables. `user_settings` holds the values of the user of the
//! client. `client_config` holds the data of the client as protobuf messages.

use std::string::String;
use std::vec::Vec;

use miden_client_core::errors::StoreError;
use miden_client_proto::{self as proto, ProtobufValue};
use miden_protocol::Word;
use rusqlite::types::FromSql;
use rusqlite::{Connection, OptionalExtension, ToSql, params};

use super::SqliteStore;
use crate::sql_error::SqlResultExt;

// CLIENT CONFIG KEYS
// ================================================================================================

pub(crate) const RPC_LIMITS_KEY: &str = "rpc_limits";
pub(crate) const TRANSACTION_ENCRYPTION_KEY_KEY: &str = "transaction_encryption_key";
pub(crate) const NOTE_TRANSPORT_CURSORS_KEY: &str = "note_transport_cursors";

/// Returns the `client_config` key of the protocol configuration for `commitment`.
pub(crate) fn protocol_config_key(commitment: Word) -> String {
    format!("protocol_config:{commitment}")
}

// SETTINGS TABLE
// ================================================================================================

/// A key-value table of the store.
#[derive(Debug, Clone, Copy)]
pub(crate) enum SettingsTable {
    /// The `user_settings` table.
    User,
    /// The `client_config` table.
    Client,
}

impl SettingsTable {
    fn name(self) -> &'static str {
        match self {
            Self::User => "user_settings",
            Self::Client => "client_config",
        }
    }
}

impl SqliteStore {
    pub(crate) fn get_setting<T: FromSql>(
        conn: &Connection,
        table: SettingsTable,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        conn.query_row(
            &format!("SELECT value FROM {} WHERE name = $1", table.name()),
            params![name],
            |row| row.get(0),
        )
        .optional()
        .into_store_error()
    }

    pub(crate) fn set_setting<T: ToSql>(
        conn: &Connection,
        table: SettingsTable,
        name: &str,
        value: &T,
    ) -> Result<(), StoreError> {
        let count = conn
            .execute(
                &format!("INSERT OR REPLACE INTO {} (name, value) VALUES ($1, $2)", table.name()),
                params![name, value],
            )
            .into_store_error()?;

        if count != 1 {
            return Err(StoreError::DatabaseError(format!(
                "writing setting {name:?} in {} affected {count} rows, expected 1",
                table.name()
            )));
        }

        Ok(())
    }

    /// Returns `true` if a row was deleted, `false` if `name` wasn't present.
    pub(crate) fn remove_setting(
        conn: &Connection,
        table: SettingsTable,
        name: &str,
    ) -> Result<bool, StoreError> {
        let count = conn
            .execute(&format!("DELETE FROM {} WHERE name = $1", table.name()), params![name])
            .into_store_error()?;

        if count > 1 {
            return Err(StoreError::DatabaseError(format!(
                "removing setting {name:?} in {} affected {count} rows, expected at most 1",
                table.name()
            )));
        }

        Ok(count == 1)
    }

    pub(crate) fn list_setting_keys(
        conn: &Connection,
        table: SettingsTable,
    ) -> Result<Vec<String>, StoreError> {
        let mut stmt =
            conn.prepare(&format!("SELECT name FROM {}", table.name())).into_store_error()?;

        stmt.query_map([], |row| row.get::<_, String>(0))
            .into_store_error()?
            .collect::<Result<Vec<String>, _>>()
            .into_store_error()
    }

    /// Reads a client setting that this store wrote as a protobuf message.
    pub(crate) fn get_client_setting<T: ProtobufValue>(
        conn: &Connection,
        name: &str,
    ) -> Result<Option<T>, StoreError> {
        let Some(bytes) = Self::get_setting::<Vec<u8>>(conn, SettingsTable::Client, name)? else {
            return Ok(None);
        };
        Ok(Some(proto::decode_unchecked(&bytes)?))
    }

    /// Writes a client setting as a protobuf message.
    pub(crate) fn set_client_setting<T: ProtobufValue>(
        conn: &Connection,
        name: &str,
        value: &T,
    ) -> Result<(), StoreError> {
        Self::set_setting(conn, SettingsTable::Client, name, &proto::encode(value))
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

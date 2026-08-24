//! The `settings` module provides methods for managing arbitrary setting values that are persisted
//! in the client's store.

use alloc::string::String;
use alloc::vec::Vec;

use miden_tx::utils::serde::{Deserializable, Serializable};

use super::Client;
use crate::errors::ClientError;

// CLIENT METHODS
// ================================================================================================

/// This section of the [Client] contains methods for:
///
/// - **Settings accessors:** Methods to get, set, and delete setting values from the store.
/// - **Default account ID:** Methods to get, set, and delete the default account ID. This is a
///   wrapper around a specific setting value.
impl<AUTH> Client<AUTH> {
    // SETTINGS ACCESSORS
    // --------------------------------------------------------------------------------------------

    /// Sets a setting value in the store. It can then be retrieved using `get_setting`.
    pub async fn set_setting<T: Serializable>(
        &mut self,
        key: String,
        value: T,
    ) -> Result<(), ClientError> {
        self.store.set_setting(key, value.to_bytes()).await.map_err(Into::into)
    }

    /// Retrieves the value for `key`, or `None` if it hasn’t been set.
    pub async fn get_setting<T: Deserializable>(
        &self,
        key: String,
    ) -> Result<Option<T>, ClientError> {
        self.store
            .get_setting(key)
            .await
            .map(|value| value.map(|value| Deserializable::read_from_bytes(&value)))?
            .transpose()
            .map_err(Into::into)
    }

    /// Deletes the setting value from the store. Returns `true` if `key` had a value set.
    pub async fn remove_setting(&mut self, key: String) -> Result<bool, ClientError> {
        self.store.remove_setting(key).await.map_err(Into::into)
    }

    /// Returns all the setting keys from the store.
    pub async fn list_setting_keys(&self) -> Result<Vec<String>, ClientError> {
        self.store.list_setting_keys().await.map_err(Into::into)
    }
}

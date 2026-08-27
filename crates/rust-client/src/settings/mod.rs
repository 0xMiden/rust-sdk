//! The `settings` module provides methods for managing arbitrary setting values that are persisted
//! in the client's store.

use alloc::string::String;
use alloc::vec::Vec;

use miden_tx::utils::serde::{Deserializable, Serializable};

use super::Client;
use crate::errors::ClientError;
use crate::store::SettingDomain;

// CLIENT METHODS
// ================================================================================================

/// This section of the [Client] contains methods to get, set and delete setting values, and to
/// list what is stored.
///
/// Settings are namespaced by a [`SettingDomain`], built with [`SettingDomain::new`]. A domain
/// groups a caller's own keys; it does not isolate them from other users of the same store.
///
/// Every domain built that way belongs to the user, so the client's own settings can neither be
/// read nor overwritten through this API, and never show up in its listings.
impl<AUTH> Client<AUTH> {
    // SETTINGS ACCESSORS
    // --------------------------------------------------------------------------------------------

    /// Sets a setting value in `domain`. It can then be retrieved using `get_setting`.
    pub async fn set_setting<T: Serializable>(
        &self,
        domain: &SettingDomain,
        key: String,
        value: T,
    ) -> Result<(), ClientError> {
        self.store.set_setting(domain, key, value.to_bytes()).await.map_err(Into::into)
    }

    /// Retrieves the value for `key` in `domain`, or `None` if it hasn’t been set.
    pub async fn get_setting<T: Deserializable>(
        &self,
        domain: &SettingDomain,
        key: String,
    ) -> Result<Option<T>, ClientError> {
        self.store
            .get_setting(domain, key)
            .await
            .map(|value| value.map(|value| Deserializable::read_from_bytes(&value)))?
            .transpose()
            .map_err(Into::into)
    }

    /// Deletes the setting value from `domain`. Returns `true` if `key` had a value set.
    pub async fn remove_setting(
        &self,
        domain: &SettingDomain,
        key: String,
    ) -> Result<bool, ClientError> {
        self.store.remove_setting(domain, key).await.map_err(Into::into)
    }

    /// Returns the setting keys held by `domain`.
    pub async fn list_setting_keys(
        &self,
        domain: &SettingDomain,
    ) -> Result<Vec<String>, ClientError> {
        self.store.list_setting_keys(domain).await.map_err(Into::into)
    }

    /// Returns the names of the domains that hold at least one setting.
    pub async fn list_setting_domains(&self) -> Result<Vec<String>, ClientError> {
        self.store.list_user_setting_domains().await.map_err(Into::into)
    }
}

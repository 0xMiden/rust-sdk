//! Stores protocol configurations by their block header commitments.

use alloc::format;

use miden_protocol::Word;
pub use miden_protocol::errors::ProtocolConfigError;
pub use miden_protocol::protocol_config::{NextProtocolConfig, ProtocolConfig};

use crate::store::{SettingScope, Store, StoreError, proto};
use crate::{Client, ClientError};

impl<AUTH> Client<AUTH> {
    /// Stores a protocol configuration for transaction execution and note screening, skipping the
    /// sync that normally delivers it.
    ///
    /// A client that reaches a node gets its configurations from [`Client::sync_state`]. This is
    /// for a client that cannot sync, such as one backed by a mock chain.
    #[cfg(feature = "testing")]
    pub async fn seed_protocol_config(&self, config: ProtocolConfig) -> Result<(), ClientError> {
        self.store
            .set_setting(
                SettingScope::Client,
                protocol_config_setting_key(config.to_commitment()),
                proto::encode(&config),
            )
            .await?;
        Ok(())
    }

    /// Returns the stored protocol configuration for the specified commitment.
    pub async fn get_protocol_config(
        &self,
        commitment: Word,
    ) -> Result<ProtocolConfig, ClientError> {
        Ok(load_protocol_config(self.store.as_ref(), commitment).await?)
    }
}

/// Returns the settings key that holds the protocol configuration for `commitment`.
///
/// A [`Store`] implementation needs this key to persist a configuration a sync returned.
pub fn protocol_config_setting_key(commitment: Word) -> alloc::string::String {
    format!("protocol_config:{commitment}")
}

pub(crate) async fn load_protocol_config(
    store: &dyn Store,
    commitment: Word,
) -> Result<ProtocolConfig, StoreError> {
    let bytes = store
        .get_setting(SettingScope::Client, protocol_config_setting_key(commitment))
        .await?
        .ok_or(StoreError::ProtocolConfigNotFound(commitment))?;
    let config: ProtocolConfig = proto::decode(&bytes)?;
    if config.to_commitment() != commitment {
        return Err(StoreError::ProtocolConfigCommitmentMismatch(commitment));
    }
    Ok(config)
}

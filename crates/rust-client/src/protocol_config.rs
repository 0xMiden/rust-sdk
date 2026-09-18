//! Stores protocol configurations by their block header commitments.

use alloc::format;

use miden_protocol::Word;
pub use miden_protocol::errors::ProtocolConfigError;
pub use miden_protocol::protocol_config::{NextProtocolConfig, ProtocolConfig};
use miden_protocol::utils::serde::{Deserializable, Serializable};

use crate::store::{SettingScope, Store, StoreError};
use crate::{Client, ClientError};

impl<AUTH> Client<AUTH> {
    /// Stores a protocol configuration for transaction execution and note screening. The
    /// configuration must match the commitment in the transaction reference block.
    pub async fn add_protocol_config(&self, config: ProtocolConfig) -> Result<(), ClientError> {
        self.store
            .set_setting(
                SettingScope::Client,
                config_key(config.to_commitment()),
                config.to_bytes(),
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

fn config_key(commitment: Word) -> alloc::string::String {
    format!("protocol_config:{commitment}")
}

pub(crate) async fn load_protocol_config(
    store: &dyn Store,
    commitment: Word,
) -> Result<ProtocolConfig, StoreError> {
    let bytes = store
        .get_setting(SettingScope::Client, config_key(commitment))
        .await?
        .ok_or(StoreError::ProtocolConfigNotFound(commitment))?;
    let config = ProtocolConfig::read_from_bytes(&bytes)?;
    if config.to_commitment() != commitment {
        return Err(StoreError::ProtocolConfigCommitmentMismatch(commitment));
    }
    Ok(config)
}

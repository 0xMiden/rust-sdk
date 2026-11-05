use miden_client::account::AccountId;
use miden_client::asset::AssetCallbackFlag;
use miden_client::keystore::Keystore;
use miden_client::store::StoreError;
use miden_client::{Client, ClientError};

/// Key of the setting that holds the ID of the fee token faucet that the store set up.
const FEE_FAUCET_SET_UP_KEY: &str = "fee_faucet_set_up";

/// Sets up the fee token faucet one time for each store.
///
/// A failure does not stop the command. The function prints a warning instead, and the next command
/// tries again.
pub(crate) async fn set_up_once<AUTH: Keystore + Sync + 'static>(client: &mut Client<AUTH>) {
    if let Err(err) = try_set_up_once(client).await {
        eprintln!("Warning: could not set up the fee token faucet: {err}");
    }
}

/// Adds the fee token faucet as a watched account and keeps its account witness up to date.
///
/// When the fee token faucet has asset callbacks, each transaction that pays a fee calls the
/// faucet. The transaction then needs the faucet state and its account witness. If the client keeps
/// them, each sync fetches the witness one time. Otherwise each transaction fetches the faucet from
/// the node. A faucet without asset callbacks is never called, so the function does not add it.
///
/// The faucet ID is in the protocol configuration, which the client stores on its first sync.
/// Before that sync, the function does nothing and a later command tries again.
async fn try_set_up_once<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
) -> Result<(), ClientError> {
    if set_up_faucet_id(client).await?.is_some() {
        return Ok(());
    }

    let sync_height = client.get_sync_height().await?;
    let Some((header, _)) = client.get_block_header_by_num(sync_height).await? else {
        return Ok(());
    };
    let protocol_config =
        match client.get_protocol_config(header.protocol_config_commitment()).await {
            Ok(protocol_config) => protocol_config,
            Err(ClientError::StoreError(StoreError::ProtocolConfigNotFound(_))) => return Ok(()),
            Err(err) => return Err(err),
        };

    let faucet_id = protocol_config.fee_asset_id().faucet_id();
    if faucet_id.asset_callback_flag() == AssetCallbackFlag::Enabled {
        if client.get_account_header(faucet_id).await?.is_none() {
            client.import_watched_account_by_id(faucet_id).await?;
        }
        client.track_account_witness(faucet_id).await?;
    }

    client.set_setting(FEE_FAUCET_SET_UP_KEY.to_string(), faucet_id).await
}

/// Returns the ID of the fee token faucet that the store set up, or `None` before the set up.
///
/// The store can keep this faucet as a watched account. A watched account is not an account of the
/// user.
pub(crate) async fn set_up_faucet_id<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
) -> Result<Option<AccountId>, ClientError> {
    client.get_setting(FEE_FAUCET_SET_UP_KEY.to_string()).await
}

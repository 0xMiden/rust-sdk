use miden_client::account::AccountId;
use miden_client::asset::AssetCallbackFlag;
use miden_client::keystore::Keystore;
use miden_client::sync::SyncSummary;
use miden_client::utils::{Deserializable, Serializable};
use miden_client::{Client, ClientError};

use crate::config::CliConfig;
use crate::errors::CliError;

/// Key of the setting that holds the ID of the fee token faucet that the store follows.
const TRACKED_FEE_FAUCET_KEY: &str = "tracked_fee_faucet";

/// Syncs the client and makes the store follow the fee token faucet if the configuration asks for
/// it.
///
/// The `track_fee_faucet` configuration field turns this on. It is off by default.
///
/// When the fee token faucet has asset callbacks, each transaction that pays a fee calls the
/// faucet. The transaction then needs the faucet state and its account witness. If the client keeps
/// them, each sync fetches the witness one time. Otherwise each transaction fetches the faucet from
/// the node. A faucet without asset callbacks is never called, so the function does not add it.
///
/// The protocol configuration can change the faucet. When it does, the function stops keeping the
/// witness of the old faucet and sets up the new one. When the configuration field is off, the
/// function stops keeping the witness of the faucet that the store follows. In both cases the old
/// faucet stays a watched account.
///
/// A failure in the set up does not stop the command. The function prints a warning instead, and
/// the next sync tries again.
pub(crate) async fn sync<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
) -> Result<SyncSummary, ClientError> {
    let summary = client.sync_state().await?;
    if let Err(err) = update_fee_faucet(client).await {
        eprintln!("Warning: could not update the fee token faucet: {err}");
    }
    Ok(summary)
}

/// Starts or stops following the fee token faucet, as the configuration field sets.
async fn update_fee_faucet<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
) -> Result<(), CliError> {
    if CliConfig::load()?.track_fee_faucet {
        track_fee_faucet(client).await?;
    } else {
        untrack_fee_faucet(client).await?;
    }
    Ok(())
}

/// Makes the store follow the fee token faucet of the protocol configuration at the sync height.
async fn track_fee_faucet<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
) -> Result<(), CliError> {
    let header = client.get_latest_block_header().await?;
    let protocol_config = client.get_protocol_config(header.protocol_config_commitment()).await?;
    let faucet_id = protocol_config.fee_asset_id().faucet_id();

    let tracked_faucet_id = tracked_faucet_id(client).await?;
    if tracked_faucet_id == Some(faucet_id) {
        return Ok(());
    }

    if let Some(old_faucet_id) = tracked_faucet_id {
        client.untrack_account_witness(old_faucet_id).await?;
    }
    if faucet_id.asset_callback_flag() == AssetCallbackFlag::Enabled {
        if client.get_account_header(faucet_id).await?.is_none() {
            client.import_watched_account_by_id(faucet_id).await?;
        }
        client.track_account_witness(faucet_id).await?;
    }

    client
        .set_setting(TRACKED_FEE_FAUCET_KEY.to_string(), faucet_id.to_bytes())
        .await?;
    Ok(())
}

/// Stops keeping the witness of the fee token faucet that the store follows, if there is one.
async fn untrack_fee_faucet<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
) -> Result<(), CliError> {
    if let Some(faucet_id) = tracked_faucet_id(client).await? {
        client.untrack_account_witness(faucet_id).await?;
        client.remove_setting(TRACKED_FEE_FAUCET_KEY.to_string()).await?;
    }
    Ok(())
}

/// Returns the ID of the fee token faucet that the store follows, or `None` if the store does not
/// follow one.
///
/// The store can keep this faucet as a watched account. A watched account is not an account of the
/// user.
pub(crate) async fn tracked_faucet_id<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
) -> Result<Option<AccountId>, CliError> {
    client
        .get_setting(TRACKED_FEE_FAUCET_KEY.to_string())
        .await?
        .map(|bytes| {
            AccountId::read_from_bytes(&bytes).map_err(|err| {
                CliError::Parse(err.into(), "Failed to parse the tracked fee faucet ID".to_string())
            })
        })
        .transpose()
}

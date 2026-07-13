use anyhow::Result;
use miden_agglayer::{AggLayerBridge, ExitRoot, UpdateGerNote};
use miden_client::testing::common::{wait_for_blocks, wait_for_tx};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::StorageMapKey;
use miden_protocol::{Hasher, ONE, Word, ZERO};

use super::{AgglayerConfig, create_agglayer_clients, setup_core_accounts};
use crate::tests::config::ClientConfig;

// TESTS
// ================================================================================================

/// Test GER update flow.
///
/// If `AGGLAYER_ACCOUNTS_DIR` is set, loads pre-deployed accounts from `.mac` files (complete
/// genesis mode). Otherwise, creates all accounts at runtime (empty genesis mode).
pub async fn test_agglayer_update_ger(client_config: ClientConfig) -> Result<()> {
    let agglayer_config = AgglayerConfig::from_env()?;
    let (mut bridge_admin, mut ger_manager, mut user) =
        create_agglayer_clients(&client_config).await?;
    let (_bridge_admin_id, ger_manager_id, bridge_id) = setup_core_accounts(
        agglayer_config.as_ref(),
        &mut bridge_admin,
        &mut ger_manager,
        &mut user,
    )
    .await?;

    // CREATE UPDATE_GER NOTE
    // --------------------------------------------------------------------------------------------
    let ger_bytes: [u8; 32] = rand::random();
    let ger = ExitRoot::from(ger_bytes);
    println!("Submitting UpdateGerNote with random GER: {ger_bytes:02x?}");
    let update_ger_note =
        UpdateGerNote::create(ger, ger_manager_id, bridge_id, ger_manager.client.rng())?;

    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![update_ger_note])
        .build()?;
    let tx_id = ger_manager.client.submit_new_transaction(ger_manager_id, tx_request).await?;
    wait_for_tx(&mut ger_manager.client, tx_id).await?;

    // WAIT FOR NETWORK ACCOUNT TO PROCESS UPDATE_GER NOTE
    // --------------------------------------------------------------------------------------------
    wait_for_blocks(&mut ger_manager.client, 5).await;

    // VERIFY GER HASH WAS STORED IN MAP
    // --------------------------------------------------------------------------------------------
    let ger_elements = ger.to_elements();
    let ger_lower: Word = ger_elements[0..4].try_into().expect("to_elements returns 8 felts");
    let ger_upper: Word = ger_elements[4..8].try_into().expect("to_elements returns 8 felts");
    let ger_key = Hasher::merge(&[ger_lower, ger_upper]);

    let stored_value = ger_manager
        .client
        .account_reader(bridge_id)
        .get_storage_map_item(
            AggLayerBridge::ger_map_slot_name().clone(),
            StorageMapKey::new(ger_key),
        )
        .await?;

    let is_registered = stored_value == Word::new([ONE, ZERO, ZERO, ZERO]);
    println!("GER registered: {is_registered}");

    assert!(is_registered, "GER was not registered in the bridge account");

    Ok(())
}

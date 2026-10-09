use anyhow::Result;
use miden_agglayer::{AggLayerBridge, ExitRoot, UpdateGerNote};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::StorageMapKey;
use miden_protocol::{Hasher, ONE, Word, ZERO};

use super::AgglayerScenario;
use crate::ClientConfig;

// TESTS
// ================================================================================================

/// Test GER update flow, against the pre-deployed accounts (see
/// [`AgglayerConfig`](super::AgglayerConfig)).
pub async fn test_agglayer_update_ger(client_config: ClientConfig) -> Result<()> {
    let AgglayerScenario {
        config: agglayer_config, mut ger_manager, ..
    } = AgglayerScenario::start(&client_config).await?;
    let (ger_manager_id, bridge_id) =
        (agglayer_config.ger_manager_id(), agglayer_config.bridge_id());

    // CREATE UPDATE_GER NOTE
    // --------------------------------------------------------------------------------------------
    let ger_bytes: [u8; 32] = rand::random();
    let ger = ExitRoot::from(ger_bytes);
    println!("Submitting UpdateGerNote with random GER: {ger_bytes:02x?}");
    let update_ger_note = UpdateGerNote::create(ger, ger_manager_id, bridge_id, ger_manager.rng())?;

    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![update_ger_note])
        .build()?;
    let tx_id = ger_manager.submit_new_transaction(ger_manager_id, tx_request).await?;
    ger_manager.wait_for_tx(tx_id).await?;

    // WAIT FOR NETWORK ACCOUNT TO PROCESS UPDATE_GER NOTE
    // --------------------------------------------------------------------------------------------
    // Polled rather than waited out: the node builds the network transaction on its own
    // schedule, which stretches with load.
    const MAX_POLL_BLOCKS: usize = 60;

    let ger_elements = ger.to_elements();
    let ger_lower: Word = ger_elements[0..4].try_into().expect("to_elements returns 8 felts");
    let ger_upper: Word = ger_elements[4..8].try_into().expect("to_elements returns 8 felts");
    let ger_key = Hasher::merge(&[ger_lower, ger_upper]);

    let mut is_registered = false;
    for _ in 0..MAX_POLL_BLOCKS {
        let stored_value = ger_manager
            .account_reader(bridge_id)
            .get_storage_map_item(
                AggLayerBridge::ger_map_slot_name().clone(),
                StorageMapKey::new(ger_key),
            )
            .await?;

        is_registered = stored_value == Word::new([ONE, ZERO, ZERO, ZERO]);
        if is_registered {
            break;
        }

        ger_manager.wait_for_blocks(1).await?;
    }

    // VERIFY GER HASH WAS STORED IN MAP
    // --------------------------------------------------------------------------------------------
    println!("GER registered: {is_registered}");

    assert!(is_registered, "GER was not registered in the bridge account");

    Ok(())
}

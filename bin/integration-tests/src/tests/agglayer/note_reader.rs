use anyhow::Result;
use miden_agglayer::{AggLayerBridge, ExitRoot, UpdateGerNote};
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::account::StorageMapKey;
use miden_protocol::{Hasher, ONE, Word, ZERO};

use super::AgglayerScenario;
use crate::ClientConfig;

// TESTS
// ================================================================================================

/// Exercises the AggLayer use case for the `InputNoteReader`: reconstructing what a bridge (a
/// network account) did by reading, in order, the notes it consumed. Note that each consumed note
/// is an event (a consumed `UPDATE_GER` = a GER registration, a `CLAIM` would be a claim event,
/// etc.).
///
/// In this test a single client submits several `UPDATE_GER` notes. The bridge consumes each as a
/// network transaction within the same batch it's created, so the note is erased from the block
/// body. The same client then reads them back via `input_note_reader(bridge_id)` in consumption
/// order. Consumed notes have no id, so they're matched by their details commitment.
///
/// After the bridge consumes each note, the test also checks that the bridge registered the GER of
/// the note in its GER map.
pub async fn test_agglayer_note_reader_reads_consumed_notes(
    client_config: ClientConfig,
) -> Result<()> {
    let AgglayerScenario {
        config: agglayer_config, mut ger_manager, ..
    } = AgglayerScenario::start(&client_config).await?;
    let (ger_manager_id, bridge_id) =
        (agglayer_config.ger_manager_id(), agglayer_config.bridge_id());

    const NOTE_COUNT: usize = 3;
    // The node builds each network transaction on its own schedule, which stretches with load.
    const MAX_POLL_BLOCKS: usize = 60;

    // Submit one UPDATE_GER note at a time. After each, wait until the reader returns exactly the
    // notes submitted so far, in order, before submitting the next, so they are consumed in
    // distinct, increasing blocks and the reader's order is asserted at every step.
    let mut expected = Vec::with_capacity(NOTE_COUNT);
    for _ in 0..NOTE_COUNT {
        let ger_bytes: [u8; 32] = rand::random();
        let ger = ExitRoot::from(ger_bytes);
        let note = UpdateGerNote::create(ger, ger_manager_id, bridge_id, ger_manager.rng())?;
        expected.push(note.details_commitment());

        let tx = TransactionRequestBuilder::new().own_output_notes(vec![note]).build()?;
        let tx_id = ger_manager.submit_new_transaction(ger_manager_id, tx).await?;
        ger_manager.wait_for_tx(tx_id).await?;

        let mut consumed = Vec::new();
        for _ in 0..MAX_POLL_BLOCKS {
            ger_manager.sync_state().await?;
            consumed.clear();
            let mut reader = ger_manager.input_note_reader(bridge_id);
            while let Some(note) = reader.next().await? {
                consumed.push(note.details_commitment());
            }
            consumed.retain(|c| expected.contains(c));
            if consumed.len() == expected.len() {
                break;
            }
            ger_manager.wait_for_blocks(1).await?;
        }

        assert_eq!(
            consumed, expected,
            "InputNoteReader should return the bridge's consumed UPDATE_GER notes in consumption order"
        );

        // The bridge consumed the note, so the synced bridge state holds the GER in its GER map.
        let ger_elements = ger.to_elements();
        let ger_lower: Word = ger_elements[0..4].try_into().expect("to_elements returns 8 felts");
        let ger_upper: Word = ger_elements[4..8].try_into().expect("to_elements returns 8 felts");
        let ger_key = Hasher::hash_double_word(&[ger_lower, ger_upper]);
        let stored_value = ger_manager
            .account_reader(bridge_id)
            .get_storage_map_item(
                AggLayerBridge::ger_map_slot_name().clone(),
                StorageMapKey::new(ger_key),
            )
            .await?;
        assert_eq!(
            stored_value,
            Word::new([ONE, ZERO, ZERO, ZERO]),
            "GER {ger_bytes:02x?} was not registered in the bridge account"
        );
    }

    Ok(())
}

use anyhow::Result;
use miden_agglayer::{ExitRoot, UpdateGerNote};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client_test_harness::ClientConfig;

use super::{AgglayerConfig, create_agglayer_clients, setup_core_accounts};

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
pub async fn test_agglayer_note_reader_reads_consumed_notes(
    client_config: ClientConfig,
) -> Result<()> {
    let agglayer_config = AgglayerConfig::from_env()?;
    let _agglayer_accounts = agglayer_config.claim()?;
    let (mut bridge_admin, mut ger_manager, mut user) =
        create_agglayer_clients(&client_config).await?;
    let (_bridge_admin_id, ger_manager_id, bridge_id) =
        setup_core_accounts(&agglayer_config, &mut bridge_admin, &mut ger_manager, &mut user)
            .await?;

    const NOTE_COUNT: usize = 3;
    // The node builds each network transaction on its own schedule, which stretches with load.
    const MAX_POLL_BLOCKS: usize = 60;

    // Submit one UPDATE_GER note at a time. After each, wait until the reader returns exactly the
    // notes submitted so far, in order, before submitting the next, so they are consumed in
    // distinct, increasing blocks and the reader's order is asserted at every step.
    let mut expected = Vec::with_capacity(NOTE_COUNT);
    for _ in 0..NOTE_COUNT {
        let ger = ExitRoot::from(rand::random::<[u8; 32]>());
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
    }

    Ok(())
}

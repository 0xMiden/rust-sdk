use anyhow::{Context, Result};
use miden_client::account::AccountType;
use miden_client::address::{Address, AddressInterface, RoutingParameters};
use miden_client::asset::FungibleAsset;
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::block::BlockNumber;
use miden_client::note::NoteType;
use miden_client::store::{InputNoteState, NoteFilter};
use miden_client::testing::common::{
    assert_account_has_single_asset,
    consume_notes,
    execute_tx_and_sync,
    insert_new_fungible_faucet,
    insert_new_wallet,
    wait_for_node,
    wait_for_tx,
};
use miden_client::transaction::TransactionRequestBuilder;

use crate::tests::config::ClientConfig;

// TRANSPORT NOTE INCLUSION PROOF AND CONSUMPTION TESTS
// ================================================================================================

/// Full end-to-end test: transport fetch → inclusion proof verification → consumption.
pub async fn test_transport_note_inclusion_proof_and_consumption(
    client_config: ClientConfig,
) -> Result<()> {
    if client_config.note_transport_endpoint.is_none() {
        eprintln!(
            "Skipping note transport test (set TEST_MIDEN_NOTE_TRANSPORT_URL or use \
             --note-transport-url to enable)"
        );
        return Ok(());
    }

    let (rpc_endpoint, rpc_timeout, ..) = client_config.as_parts();
    let sender_config = ClientConfig::new(rpc_endpoint.clone(), rpc_timeout)
        .with_note_transport_endpoint(client_config.note_transport_endpoint.clone());
    let recipient_config = ClientConfig::new(rpc_endpoint, rpc_timeout)
        .with_note_transport_endpoint(client_config.note_transport_endpoint);

    let (sender_builder, sender_keystore) = sender_config
        .into_client_builder()
        .await
        .context("failed to get sender builder")?;
    let mut sender = sender_builder.build().await.context("failed to build sender")?;
    let (recipient_builder, recipient_keystore) = recipient_config
        .into_client_builder()
        .await
        .context("failed to get recipient builder")?;
    let mut recipient = recipient_builder.build().await.context("failed to build recipient")?;

    wait_for_node(&mut sender).await;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut sender,
        AccountType::Private,
        &sender_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .context("failed to insert faucet")?;

    let (recipient_account, _) = insert_new_wallet(
        &mut recipient,
        AccountType::Private,
        &recipient_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .context("failed to insert wallet")?;

    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    // Initial sync
    recipient.sync_state().await.context("recipient initial sync")?;

    // Mint private note
    let fungible_asset = FungibleAsset::new(faucet_account.id(), 100).context("asset")?;
    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            fungible_asset,
            recipient_account.id(),
            NoteType::Private,
            sender.rng(),
        )
        .context("build mint tx")?;
    let note = tx_request
        .expected_output_own_notes()
        .last()
        .cloned()
        .context("expected output note missing")?;

    execute_tx_and_sync(&mut sender, faucet_account.id(), tx_request)
        .await
        .context("mint tx failed")?;

    // Send via transport
    sender
        .send_private_note_with_block_hint(note.clone(), &recipient_address, BlockNumber::from(0))
        .await
        .context("send_private_note failed")?;

    // Recipient syncs (transport fetch + state sync)
    recipient.sync_state().await.context("recipient sync")?;

    // Verify note state
    let notes = recipient.get_input_notes(NoteFilter::All).await?;
    let received = notes
        .iter()
        .find(|n| n.id() == Some(note.id()))
        .context("minted note not received by recipient")?;
    assert!(
        matches!(received.state(), InputNoteState::Committed(..)),
        "note should be committed, got: {:?}",
        received.state()
    );
    assert!(received.inclusion_proof().is_some(), "note should have inclusion proof");

    // Verify consumability
    let consumable = recipient.get_consumable_notes(Some(recipient_account.id())).await?;
    assert!(
        consumable.iter().any(|(n, _)| n.id() == Some(note.id())),
        "minted note should be consumable",
    );

    // Consume the note
    let tx_id = consume_notes(&mut recipient, recipient_account.id(), &[note]).await;
    wait_for_tx(&mut recipient, tx_id).await?;

    // Verify balance
    assert_account_has_single_asset(&recipient, recipient_account.id(), faucet_account.id(), 100)
        .await;

    Ok(())
}

/// Tests fetching and consuming multiple notes committed in different blocks.
pub async fn test_transport_multiple_notes_different_blocks(
    client_config: ClientConfig,
) -> Result<()> {
    if client_config.note_transport_endpoint.is_none() {
        eprintln!(
            "Skipping note transport test (set TEST_MIDEN_NOTE_TRANSPORT_URL or use \
             --note-transport-url to enable)"
        );
        return Ok(());
    }

    let (rpc_endpoint, rpc_timeout, ..) = client_config.as_parts();
    let sender_config = ClientConfig::new(rpc_endpoint.clone(), rpc_timeout)
        .with_note_transport_endpoint(client_config.note_transport_endpoint.clone());
    let recipient_config = ClientConfig::new(rpc_endpoint, rpc_timeout)
        .with_note_transport_endpoint(client_config.note_transport_endpoint);

    let (sender_builder, sender_keystore) = sender_config
        .into_client_builder()
        .await
        .context("failed to get sender builder")?;
    let mut sender = sender_builder.build().await.context("failed to build sender")?;
    let (recipient_builder, recipient_keystore) = recipient_config
        .into_client_builder()
        .await
        .context("failed to get recipient builder")?;
    let mut recipient = recipient_builder.build().await.context("failed to build recipient")?;

    wait_for_node(&mut sender).await;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut sender,
        AccountType::Private,
        &sender_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .context("failed to insert faucet")?;

    let (recipient_account, _) = insert_new_wallet(
        &mut recipient,
        AccountType::Private,
        &recipient_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .context("failed to insert wallet")?;

    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    // Initial sync
    recipient.sync_state().await.context("recipient initial sync")?;

    // Mint 3 notes with different amounts, each committed in a separate block via
    // execute_tx_and_sync (which waits for each tx to be committed before returning).
    let amounts = [10u64, 20, 30];
    let mut minted_notes = Vec::new();
    for amount in amounts {
        let fungible_asset =
            FungibleAsset::new(faucet_account.id(), amount).context("failed to create asset")?;
        let tx_request = TransactionRequestBuilder::new()
            .build_mint_fungible_asset(
                fungible_asset,
                recipient_account.id(),
                NoteType::Private,
                sender.rng(),
            )
            .context("build mint tx")?;
        let note = tx_request
            .expected_output_own_notes()
            .last()
            .cloned()
            .context("expected output note missing")?;
        execute_tx_and_sync(&mut sender, faucet_account.id(), tx_request)
            .await
            .context("mint tx failed")?;
        minted_notes.push(note);
    }

    // Send all 3 notes via transport
    for note in &minted_notes {
        sender
            .send_private_note_with_block_hint(
                note.clone(),
                &recipient_address,
                BlockNumber::from(0),
            )
            .await
            .context("send_private_note failed")?;
    }

    // Recipient syncs
    recipient.sync_state().await.context("recipient sync")?;

    // Verify all minted notes received and committed
    let input_notes = recipient.get_input_notes(NoteFilter::All).await?;
    let minted_ids: std::collections::HashSet<_> = minted_notes.iter().map(|n| n.id()).collect();
    let received: Vec<_> = input_notes
        .iter()
        .filter(|n| n.id().is_some_and(|id| minted_ids.contains(&id)))
        .collect();
    assert_eq!(
        received.len(),
        minted_ids.len(),
        "all minted notes should be received, got {}/{}",
        received.len(),
        minted_ids.len(),
    );
    for input_note in &received {
        assert!(
            matches!(input_note.state(), InputNoteState::Committed(..)),
            "note should be committed, got: {:?}",
            input_note.state()
        );
        assert!(input_note.inclusion_proof().is_some(), "note should have inclusion proof");
    }

    // Verify our minted notes were committed across at least 2 different blocks.
    let mut block_nums: Vec<_> = received
        .iter()
        .filter_map(|n| n.inclusion_proof().map(|p| p.location().block_num()))
        .collect();
    block_nums.sort();
    block_nums.dedup();
    assert!(
        block_nums.len() >= 2,
        "expected at least 2 different commit blocks, got {}",
        block_nums.len()
    );

    // Verify all minted notes are consumable.
    let consumable = recipient.get_consumable_notes(Some(recipient_account.id())).await?;
    let consumable_minted = consumable
        .iter()
        .filter(|(n, _)| n.id().is_some_and(|id| minted_ids.contains(&id)))
        .count();
    assert_eq!(
        consumable_minted,
        minted_ids.len(),
        "all minted notes should be consumable, got {}/{}",
        consumable_minted,
        minted_ids.len(),
    );

    // Consume all notes
    let tx_id = consume_notes(&mut recipient, recipient_account.id(), &minted_notes).await;
    wait_for_tx(&mut recipient, tx_id).await?;

    // Verify total balance (10 + 20 + 30 = 60)
    assert_account_has_single_asset(&recipient, recipient_account.id(), faucet_account.id(), 60)
        .await;

    Ok(())
}

/// Tests that a note sent via transport before being committed on-chain starts as Expected,
/// then transitions to Committed once the mint tx is executed and synced.
pub async fn test_transport_note_not_yet_committed(client_config: ClientConfig) -> Result<()> {
    if client_config.note_transport_endpoint.is_none() {
        eprintln!(
            "Skipping note transport test (set TEST_MIDEN_NOTE_TRANSPORT_URL or use \
             --note-transport-url to enable)"
        );
        return Ok(());
    }

    let (rpc_endpoint, rpc_timeout, ..) = client_config.as_parts();
    let sender_config = ClientConfig::new(rpc_endpoint.clone(), rpc_timeout)
        .with_note_transport_endpoint(client_config.note_transport_endpoint.clone());
    let recipient_config = ClientConfig::new(rpc_endpoint, rpc_timeout)
        .with_note_transport_endpoint(client_config.note_transport_endpoint);

    let (sender_builder, sender_keystore) = sender_config
        .into_client_builder()
        .await
        .context("failed to get sender builder")?;
    let mut sender = sender_builder.build().await.context("failed to build sender")?;
    let (recipient_builder, recipient_keystore) = recipient_config
        .into_client_builder()
        .await
        .context("failed to get recipient builder")?;
    let mut recipient = recipient_builder.build().await.context("failed to build recipient")?;

    wait_for_node(&mut sender).await;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut sender,
        AccountType::Private,
        &sender_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .context("failed to insert faucet")?;

    let (recipient_account, _) = insert_new_wallet(
        &mut recipient,
        AccountType::Private,
        &recipient_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .context("failed to insert wallet")?;

    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    // Initial sync
    recipient.sync_state().await.context("recipient initial sync")?;

    // Build mint tx and extract the note BEFORE executing
    let fungible_asset = FungibleAsset::new(faucet_account.id(), 100).context("asset")?;
    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            fungible_asset,
            recipient_account.id(),
            NoteType::Private,
            sender.rng(),
        )
        .context("build mint tx")?;
    let note = tx_request
        .expected_output_own_notes()
        .last()
        .cloned()
        .context("expected output note missing")?;

    // Send via transport BEFORE the note is committed on-chain
    sender
        .send_private_note_with_block_hint(note.clone(), &recipient_address, BlockNumber::from(0))
        .await
        .context("send_private_note failed")?;

    // Recipient syncs — transport fetch finds the note, but it's not on chain yet
    recipient.sync_state().await.context("recipient sync (pre-commit)")?;

    let notes = recipient.get_input_notes(NoteFilter::All).await?;
    let received = notes
        .iter()
        .find(|n| n.id() == Some(note.id()))
        .context("note not received by recipient via transport")?;
    assert!(
        matches!(received.state(), InputNoteState::Expected(..)),
        "note should be Expected (not yet on chain), got: {:?}",
        received.state()
    );
    assert!(received.inclusion_proof().is_none(), "no inclusion proof before commit");

    // Our note shouldn't be consumable yet (pre-commit)
    let consumable = recipient.get_consumable_notes(Some(recipient_account.id())).await?;
    assert!(
        !consumable.iter().any(|(n, _)| n.id() == Some(note.id())),
        "minted note should not be consumable before commit",
    );

    // Now execute the mint tx — note commits on chain
    execute_tx_and_sync(&mut sender, faucet_account.id(), tx_request)
        .await
        .context("mint tx failed")?;

    // Recipient syncs again — note tag tracking finds it on chain
    recipient.sync_state().await.context("recipient sync (post-commit)")?;

    let notes = recipient.get_input_notes(NoteFilter::All).await?;
    let received = notes
        .iter()
        .find(|n| n.id() == Some(note.id()))
        .context("note not present after post-commit sync")?;
    assert!(
        matches!(received.state(), InputNoteState::Committed(..)),
        "note should now be Committed, got: {:?}",
        received.state()
    );
    assert!(received.inclusion_proof().is_some(), "should have inclusion proof after commit");

    // Consume the note
    let tx_id = consume_notes(&mut recipient, recipient_account.id(), &[note]).await;
    wait_for_tx(&mut recipient, tx_id).await?;

    assert_account_has_single_asset(&recipient, recipient_account.id(), faucet_account.id(), 100)
        .await;

    Ok(())
}

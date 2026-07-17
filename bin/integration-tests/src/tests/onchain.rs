use std::collections::BTreeMap;

use anyhow::{Context, Result};
use miden_client::account::{AccountType, build_wallet_id};
use miden_client::asset::{Asset, AssetAmount, FungibleAsset};
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::keystore::Keystore;
use miden_client::note::standards::NoteSyncHint;
use miden_client::note::{
    BlockNumber,
    Note,
    NoteAttachment,
    NoteAttachmentScheme,
    NoteAttachments,
    NoteFile,
    NoteType,
    P2idNote,
};
use miden_client::rpc::{AcceptHeaderError, RpcError};
use miden_client::store::{InputNoteState, NoteFilter, TransactionFilter};
use miden_client::sync::NoteTagSource;
use miden_client::testing::common::*;
use miden_client::transaction::{
    InputNote,
    PaymentNoteDescription,
    TransactionRequestBuilder,
    TransactionStatus,
};
use miden_client::{ClientError, EMPTY_WORD, Word};
use rand::Rng;
use tracing::info;

use crate::tests::config::ClientConfig;

// TESTS
// ================================================================================================

pub async fn test_onchain_notes_flow(client_config: ClientConfig) -> Result<()> {
    // Client 1 is an private faucet which will mint an onchain note for client 2
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    // Client 2 is an private account which will consume the note that it will sync from the node
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    // Client 3 will be transferred part of the assets by client 2's account
    let (mut client_3, keystore_3) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client_3).await;

    // Create faucet account
    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Private,
        &keystore_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    // Create regular accounts
    let (basic_wallet_1, ..) =
        insert_new_wallet(&mut client_2, AccountType::Private, &keystore_2, RPO_FALCON_SCHEME_ID)
            .await?;

    // Create regular accounts
    let (basic_wallet_2, ..) =
        insert_new_wallet(&mut client_3, AccountType::Private, &keystore_3, RPO_FALCON_SCHEME_ID)
            .await?;
    client_1.sync_state().await?;
    client_2.sync_state().await?;

    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        FungibleAsset::new(faucet_account.id(), MINT_AMOUNT)?,
        basic_wallet_1.id(),
        NoteType::Public,
        client_1.rng(),
    )?;
    let note = tx_request
        .expected_output_own_notes()
        .pop()
        .with_context(|| "no expected output notes found in onchain transaction from faucet")?
        .clone();
    execute_tx_and_sync(&mut client_1, faucet_account.id(), tx_request).await?;

    // Client 2's account should receive the note here:
    client_2.sync_state().await?;

    // Assert that the note is the same
    let received_note: InputNote = client_2
        .get_input_note(note.id())
        .await?
        .with_context(|| format!("Note {} not found in client_2", note.id()))?
        .try_into()?;
    assert_eq!(received_note.note().id(), note.id());

    // TODO: revisit this.
    // The received note has the uri of the note stored in the node, so it may not match with the
    // original note.
    // assert_eq!(received_note.note(), &note);

    // consume the note
    let tx_id =
        consume_notes(&mut client_2, basic_wallet_1.id(), &[received_note.note().clone()]).await;
    wait_for_tx(&mut client_2, tx_id).await?;
    assert_account_has_single_asset(
        &client_2,
        basic_wallet_1.id(),
        faucet_account.id(),
        MINT_AMOUNT,
    )
    .await;

    let p2id_asset = FungibleAsset::new(faucet_account.id(), TRANSFER_AMOUNT)?;
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(
            vec![p2id_asset.into()],
            basic_wallet_1.id(),
            basic_wallet_2.id(),
        ),
        NoteType::Public,
        client_2.rng(),
    )?;
    execute_tx_and_sync(&mut client_2, basic_wallet_1.id(), tx_request).await?;

    // Create a note for client 3 that is already consumed before syncing
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(
            vec![p2id_asset.into()],
            basic_wallet_1.id(),
            basic_wallet_2.id(),
        )
        .with_reclaim_height(1.into()),
        NoteType::Public,
        client_2.rng(),
    )?;
    let note = tx_request
        .expected_output_own_notes()
        .pop()
        .with_context(|| "no expected output notes found in onchain transaction from basic wallet")?
        .clone();
    execute_tx_and_sync(&mut client_2, basic_wallet_1.id(), tx_request).await?;

    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note.clone()])?;
    execute_tx_and_sync(&mut client_2, basic_wallet_1.id(), tx_request).await?;

    // sync client 3 (basic account 2)
    client_3.sync_state().await?;

    // client 3 should have two notes, the one directed to them and the one consumed by client 2
    // (which should come from the tag added)
    assert_eq!(client_3.get_input_notes(NoteFilter::Committed).await?.len(), 1);
    assert_eq!(client_3.get_input_notes(NoteFilter::Consumed).await?.len(), 1);

    let note = client_3
        .get_input_notes(NoteFilter::Committed)
        .await?
        .first()
        .with_context(|| "no committed input notes found")?
        .clone()
        .try_into()?;

    let tx_id = consume_notes(&mut client_3, basic_wallet_2.id(), &[note]).await;
    wait_for_tx(&mut client_3, tx_id).await?;
    assert_account_has_single_asset(
        &client_3,
        basic_wallet_2.id(),
        faucet_account.id(),
        TRANSFER_AMOUNT,
    )
    .await;
    Ok(())
}

pub async fn test_onchain_accounts(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client_2).await;

    let (faucet_account_header, secret_key) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Public,
        &keystore_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (first_regular_account, ..) =
        insert_new_wallet(&mut client_1, AccountType::Private, &keystore_1, RPO_FALCON_SCHEME_ID)
            .await?;

    let (second_client_first_regular_account, ..) =
        insert_new_wallet(&mut client_2, AccountType::Private, &keystore_2, RPO_FALCON_SCHEME_ID)
            .await?;

    let target_account_id = first_regular_account.id();
    let second_client_target_account_id = second_client_first_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    keystore_2.add_key(&secret_key, faucet_account_id).await?;
    client_2.add_account(&faucet_account_header, false).await?;

    // First Mint necessary token
    info!(account_id = %target_account_id, faucet_id = %faucet_account_id, "First client minting note");
    client_1.sync_state().await?;
    let (tx_id, note) =
        mint_note(&mut client_1, target_account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // Update the state in the other client and ensure the onchain faucet commitment is consistent
    // between clients
    client_2.sync_state().await?;

    let (client_1_faucet, _) = client_1
        .account_reader(faucet_account_header.id())
        .header()
        .await
        .context("failed to find faucet account in client 1 after sync")?;
    let (client_2_faucet, _) = client_2
        .account_reader(faucet_account_header.id())
        .header()
        .await
        .context("failed to find faucet account in client 2 after sync")?;

    assert_eq!(client_1_faucet.to_commitment(), client_2_faucet.to_commitment());

    // Now use the faucet in the second client to mint to its own account
    info!(account_id = %second_client_target_account_id, faucet_id = %faucet_account_id, "Second client minting note");
    let (tx_id, second_client_note) = mint_note(
        &mut client_2,
        second_client_target_account_id,
        faucet_account_id,
        NoteType::Private,
    )
    .await;
    wait_for_tx(&mut client_2, tx_id).await?;

    // Update the state in the other client and ensure the onchain faucet commitment is consistent
    // between clients
    client_1.sync_state().await?;

    info!(account_id = %target_account_id, "Consuming note on first client");
    let tx_id = consume_notes(&mut client_1, target_account_id, &[note]).await;
    wait_for_tx(&mut client_1, tx_id).await?;
    assert_account_has_single_asset(&client_1, target_account_id, faucet_account_id, MINT_AMOUNT)
        .await;
    let tx_id =
        consume_notes(&mut client_2, second_client_target_account_id, &[second_client_note]).await;
    wait_for_tx(&mut client_2, tx_id).await?;
    assert_account_has_single_asset(
        &client_2,
        second_client_target_account_id,
        faucet_account_id,
        MINT_AMOUNT,
    )
    .await;

    let (client_1_faucet, _) =
        client_1
            .account_reader(faucet_account_header.id())
            .header()
            .await
            .context("failed to find faucet account in client 1 after consume transactions")?;
    let (client_2_faucet, _) =
        client_2
            .account_reader(faucet_account_header.id())
            .header()
            .await
            .context("failed to find faucet account in client 2 after consume transactions")?;

    assert_eq!(client_1_faucet.to_commitment(), client_2_faucet.to_commitment());

    // Now we'll try to do a p2id transfer from an account of one client to the other one
    let from_account_id = target_account_id;
    let to_account_id = second_client_target_account_id;

    // get initial balances
    let from_account_balance = client_1
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find from account for balance check")?;
    let to_account_balance = client_2
        .account_reader(to_account_id)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find to account for balance check")?;

    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT)?;

    info!(from = %from_account_id, to = %to_account_id, amount = TRANSFER_AMOUNT, "Running P2ID transaction");
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(vec![Asset::Fungible(asset)], from_account_id, to_account_id),
        NoteType::Public,
        client_1.rng(),
    )?;
    execute_tx_and_sync(&mut client_1, from_account_id, tx_request).await?;

    // sync on second client until we receive the note
    info!("Syncing state on second client");
    client_2.sync_state().await?;
    let notes = client_2.get_input_notes(NoteFilter::Committed).await?;

    //Import the note on the first client so that we can later check its consumer account
    let note_id = notes[0].id().expect("committed note has metadata so id() is Some");
    client_1.import_notes(&[NoteFile::NoteId(note_id)]).await?;

    // Consume the note
    info!(note_id = %note_id, account_id = %to_account_id, "Consuming note on second client");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![notes[0].clone().try_into().unwrap()])?;
    execute_tx_and_sync(&mut client_2, to_account_id, tx_request).await?;

    // sync on first client
    info!("Syncing state on first client");
    client_1.sync_state().await?;

    // Check that the client doesn't know who consumed the note. A `ConsumedExternal` note has no
    // metadata, so look it up by its details commitment rather than its note ID.
    let details_commitment = notes[0].details_commitment();
    let input_note = client_1
        .get_input_notes(NoteFilter::DetailsCommitments(vec![details_commitment]))
        .await?
        .pop()
        .with_context(|| format!("input note {note_id} not found"))?;
    assert!(matches!(input_note.state(), InputNoteState::ConsumedExternal { .. }));

    let new_from_account_balance = client_1
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find from account after transfer")?;
    let new_to_account_balance = client_2
        .account_reader(to_account_id)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find to account after transfer")?;

    assert_eq!(
        new_from_account_balance,
        (from_account_balance - AssetAmount::new(TRANSFER_AMOUNT).unwrap()).unwrap()
    );
    assert_eq!(
        new_to_account_balance,
        (to_account_balance + AssetAmount::new(TRANSFER_AMOUNT).unwrap()).unwrap()
    );
    Ok(())
}

pub async fn test_import_account_by_id(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client_1).await;

    let mut user_seed = [0u8; 32];
    client_1.rng().fill_bytes(&mut user_seed);

    let (faucet_account_header, _) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Public,
        &keystore_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (first_regular_account, secret_key) = insert_new_wallet_with_seed(
        &mut client_1,
        AccountType::Public,
        &keystore_1,
        user_seed,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let target_account_id = first_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First mint and consume in the first client
    let tx_id =
        mint_and_consume(&mut client_1, target_account_id, faucet_account_id, NoteType::Public)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // Mint a note for the second client
    let (tx_id, note) =
        mint_note(&mut client_1, target_account_id, faucet_account_id, NoteType::Public).await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // Import the public account by id
    let built_wallet_id =
        build_wallet_id(user_seed, &secret_key.public_key(), AccountType::Public)?;
    assert_eq!(built_wallet_id, first_regular_account.id());
    client_2.import_account_by_id(built_wallet_id).await?;
    keystore_2.add_key(&secret_key, built_wallet_id).await?;

    let original_commitment = client_1
        .account_reader(first_regular_account.id())
        .commitment()
        .await
        .with_context(|| {
            format!("Original account {} not found in client_1", first_regular_account.id())
        })?;
    let imported_commitment = client_2
        .account_reader(first_regular_account.id())
        .commitment()
        .await
        .with_context(|| {
            format!("Imported account {} not found in client_2", first_regular_account.id())
        })?;
    assert_eq!(imported_commitment, original_commitment);

    // Now use the wallet in the second client to consume the generated note
    info!(account_id = %target_account_id, "Second client consuming note");
    client_2.sync_state().await?;
    let tx_id = consume_notes(&mut client_2, target_account_id, &[note]).await;
    wait_for_tx(&mut client_2, tx_id).await?;
    assert_account_has_single_asset(
        &client_2,
        target_account_id,
        faucet_account_id,
        MINT_AMOUNT * 2,
    )
    .await;
    Ok(())
}

/// Watched-account flow:
///   - `client_1` owns the wallet and faucet, executes transactions.
///   - `client_2` watches the wallet via `import_watched_account_by_id` (no note tag).
///   - After `client_1` runs another mint+consume on the wallet, `client_2` should observe (a) the
///     new account commitment matching `client_1`, and (b) no output note record for the account's
///     txs (watched accounts track on-chain state, not their note outputs).
pub async fn test_import_watched_account_by_id(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    let (mut client_2, _keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client_1).await;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Public,
        &keystore_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (wallet, _) =
        insert_new_wallet(&mut client_1, AccountType::Public, &keystore_1, RPO_FALCON_SCHEME_ID)
            .await?;
    let wallet_id = wallet.id();
    let faucet_id = faucet_account.id();

    // Fill the wallet with an asset so the watched account has non-trivial state.
    let tx_id = mint_and_consume(&mut client_1, wallet_id, faucet_id, NoteType::Public).await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // client_2 starts watching the wallet.
    client_2.import_watched_account_by_id(wallet_id).await?;

    let initial_source_commitment = client_1.account_reader(wallet_id).commitment().await?;
    let initial_watched_commitment = client_2.account_reader(wallet_id).commitment().await?;
    assert_eq!(
        initial_watched_commitment, initial_source_commitment,
        "watched account commitment should match source after watch",
    );

    let watched_record = client_2
        .test_store()
        .get_account(wallet_id)
        .await?
        .context("watched account should be tracked in client_2's store")?;
    assert!(watched_record.is_watched(), "watched account must be marked as watched");

    let tags = client_2.test_store().get_note_tags().await?;
    assert!(
        !tags
            .iter()
            .any(|t| matches!(t.source, NoteTagSource::Account(id) if id == wallet_id)),
        "watched account must not register a per-account note tag",
    );

    // client_1 mints another note to the wallet and consumes it, giving client_2's watched view
    // fresh activity to track. No per-account tag is registered, so client_2 watches the account
    // only through its on-chain state.
    let (tx_id, mint_note) = mint_note(&mut client_1, wallet_id, faucet_id, NoteType::Public).await;
    wait_for_tx(&mut client_1, tx_id).await?;
    let consume_tx_id =
        consume_notes(&mut client_1, wallet_id, std::slice::from_ref(&mint_note)).await;
    wait_for_tx(&mut client_1, consume_tx_id).await?;

    client_2.sync_state().await?;

    let updated_source_commitment = client_1.account_reader(wallet_id).commitment().await?;
    let updated_watched_commitment = client_2.account_reader(wallet_id).commitment().await?;
    assert_eq!(
        updated_watched_commitment, updated_source_commitment,
        "watched commitment should track source after sync",
    );
    assert_ne!(
        updated_watched_commitment, initial_watched_commitment,
        "watched account state should have advanced",
    );

    // A watched account surfaces no output-note records from its transactions.
    let watched_output_notes = client_2.test_store().get_output_notes(NoteFilter::All).await?;
    assert!(
        watched_output_notes.is_empty(),
        "watched client must not surface output notes from the account's txs",
    );

    // Switching an already-tracked watched account to native (or vice versa) is not supported.
    let err = client_2
        .import_account_by_id(wallet_id)
        .await
        .expect_err("import_account_by_id must reject already-tracked watched accounts");
    assert!(
        matches!(err, ClientError::AccountWatchedMismatch(id) if id == wallet_id),
        "expected AccountWatchedMismatch, got {err:?}",
    );

    // Re-importing in the same mode is still a no-op overwrite, and the account stays
    // watched with no per-account tag.
    client_2.import_watched_account_by_id(wallet_id).await?;
    let record = client_2
        .test_store()
        .get_account(wallet_id)
        .await?
        .context("account should still be tracked after re-import")?;
    assert!(record.is_watched(), "account must remain watched");
    let tags = client_2.test_store().get_note_tags().await?;
    assert!(
        !tags
            .iter()
            .any(|t| matches!(t.source, NoteTagSource::Account(id) if id == wallet_id)),
        "watched account must not have a per-account note tag",
    );

    Ok(())
}

pub async fn test_incorrect_genesis(client_config: ClientConfig) -> Result<()> {
    let (builder, _) = client_config.into_client_builder().await?;
    let mut client = builder.build().await?;

    // Set an incorrect genesis commitment
    client.test_rpc_api().set_genesis_commitment(EMPTY_WORD).await?;

    // This request would always be valid as it requests the chain tip. But it should fail
    // because the genesis commitment in the request header does not match the one in the node.
    let result = client.test_rpc_api().get_block_header_by_number(None, false).await;

    match result {
        Err(RpcError::AcceptHeaderError(AcceptHeaderError::NoSupportedMediaRange(_))) => Ok(()),
        Ok(_) => anyhow::bail!("grpc request was unexpectedly successful"),
        _ => anyhow::bail!("expected accept header error"),
    }
}

/// Tests that consumed notes are returned in the correct transaction order when multiple
/// consume transactions for the same account are included in the same block.
///
/// The test mints 3 notes, then submits 3 consume transactions as a single proven batch
/// so they land in the same block. After syncing, it verifies that `InputNoteReader` returns the
/// notes in submission order.
pub async fn test_consumed_note_ordering(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    wait_for_node(&mut client).await;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (wallet_account, ..) =
        insert_new_wallet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;

    client.sync_state().await?;

    // Pre-batch: put the wallet on-chain so the wallet's first batch-tx delta is partial, not
    // full-state — the batch apply path rejects full-state deltas.
    let bootstrap_tx_id =
        mint_and_consume(&mut client, wallet_account.id(), faucet_account.id(), NoteType::Private)
            .await;
    wait_for_tx(&mut client, bootstrap_tx_id).await?;
    client.sync_state().await?;

    // Mint 3 notes, each in a separate transaction.
    let mut minted_notes = Vec::new();
    for i in 0..3 {
        let (tx_id, note) =
            mint_note(&mut client, wallet_account.id(), faucet_account.id(), NoteType::Private)
                .await;
        info!(tx_id = %tx_id, note_id = %note.id(), index = i, "Minted note");
        wait_for_tx(&mut client, tx_id).await?;
        minted_notes.push(note);
    }
    client.sync_state().await?;

    // Build a consume request per minted note and submit them as a single proven batch.
    let mut batch = client.new_transaction_batch();
    for (i, note) in minted_notes.iter().enumerate() {
        let tx_request = TransactionRequestBuilder::new()
            .build_consume_notes(vec![note.clone()])
            .unwrap();
        info!(note_id = %note.id(), index = i, "Pushing consume tx into batch");
        batch = batch.push(wallet_account.id(), tx_request).await?;
    }
    let submission_tip = batch.submit().await?;
    info!(submission_tip = submission_tip.as_u32(), "Submitted 3-tx consume batch");

    // Sync until the three consume txs are committed, then capture their batch block.
    let mut batch_block = None;
    for _ in 0..15 {
        client.sync_state().await?;

        let mut txs_per_block: BTreeMap<BlockNumber, usize> = BTreeMap::new();
        for tx in client.get_transactions(TransactionFilter::All).await? {
            if tx.details.account_id != wallet_account.id() {
                continue;
            }
            if let TransactionStatus::Committed { block_number, .. } = tx.status {
                *txs_per_block.entry(block_number).or_default() += 1;
            }
        }

        if let Some((&block, _)) = txs_per_block.iter().find(|&(_, &count)| count == 3) {
            batch_block = Some(block);
            break;
        }

        wait_for_blocks(&mut client, 1).await;
    }
    let batch_block =
        batch_block.with_context(|| "3 consume txs were not committed in the same block")?;
    info!(
        batch_block = batch_block.as_u32(),
        "All 3 consume txs committed in the same block"
    );

    // Verify all 3 notes are marked as consumed.
    let consumed_notes = client.get_input_notes(NoteFilter::Consumed).await?;
    assert!(
        consumed_notes.len() >= 3,
        "Expected at least 3 consumed notes, got {}",
        consumed_notes.len()
    );

    // Collect consumed notes via InputNoteReader for this wallet.
    let mut reader = client.input_note_reader(wallet_account.id());
    let mut reader_notes = Vec::new();
    while let Some(note) = reader.next().await? {
        reader_notes.push(note);
    }
    assert!(
        reader_notes.len() >= 3,
        "Expected at least 3 notes from reader, got {}",
        reader_notes.len()
    );

    // Extract the nullifier block height from a consumed note state
    let consumed_block_height = |note: &miden_client::store::InputNoteRecord| -> Option<u32> {
        match note.state() {
            InputNoteState::ConsumedAuthenticatedLocal(s) => {
                Some(s.nullifier_block_height.as_u32())
            },
            InputNoteState::ConsumedUnauthenticatedLocal(s) => {
                Some(s.nullifier_block_height.as_u32())
            },
            InputNoteState::ConsumedExternal(s) => Some(s.nullifier_block_height.as_u32()),
            _ => None,
        }
    };
    for window in reader_notes.windows(2) {
        let a = &window[0];
        let b = &window[1];
        let a_block = consumed_block_height(a).expect("consumed note should have block height");
        let b_block = consumed_block_height(b).expect("consumed note should have block height");
        assert!(
            a_block <= b_block,
            "Notes should be ordered by block height: note {:?} at block {} came before note {:?} at block {}",
            a.id(),
            a_block,
            b.id(),
            b_block,
        );
    }

    // The three consumed notes must appear in submission order.
    let reader_note_ids: Vec<_> = reader_notes.iter().filter_map(|n| n.id()).collect();
    let positions: Vec<_> = minted_notes
        .iter()
        .map(|note| {
            reader_note_ids
                .iter()
                .position(|id| *id == note.id())
                .with_context(|| format!("Minted note {} not found in reader output", note.id()))
        })
        .collect::<Result<Vec<_>>>()?;
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "Notes should appear in submission order, but got positions: {positions:?}"
    );

    Ok(())
}

/// A client that only *watches* an account (no note tag registered) recovers a committed
/// public note the account consumed authenticated, even though it never discovered the note by tag.
///
/// The node attaches a `consumed_note_refs` entry (the note's id, mapped from the input nullifier)
/// to the consumer's transaction. The client reads it during sync, fetches the full body via
/// `get_notes_by_id`, and surfaces the note through `input_note_reader`.
pub async fn test_watched_account_recovers_consumed_public_note(
    client_config: ClientConfig,
) -> Result<()> {
    let (mut client_a, keystore_a) = client_config.clone().into_client().await?;
    let (mut client_b, _keystore_b) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client_a).await;

    let (faucet, _) = insert_new_fungible_faucet(
        &mut client_a,
        AccountType::Public,
        &keystore_a,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (consumer, ..) =
        insert_new_wallet(&mut client_a, AccountType::Public, &keystore_a, RPO_FALCON_SCHEME_ID)
            .await?;
    let consumer_id = consumer.id();
    let faucet_id = faucet.id();

    // Put the consumer on-chain, then have B watch it. No per-account note tag is registered, so
    // B can only learn about consumed notes from the consumer's transactions.
    let bootstrap_tx =
        mint_and_consume(&mut client_a, consumer_id, faucet_id, NoteType::Public).await;
    wait_for_tx(&mut client_a, bootstrap_tx).await?;
    client_a.sync_state().await?;
    client_b.import_watched_account_by_id(consumer_id).await?;
    client_b.sync_state().await?;

    // A mints a public note to the consumer, lets it commit, then consumes it (authenticated). B
    // never tracked this note's tag, so the only trace it can get is the consuming transaction.
    let (mint_tx, note) = mint_note(&mut client_a, consumer_id, faucet_id, NoteType::Public).await;
    wait_for_tx(&mut client_a, mint_tx).await?;
    let consume_tx = consume_notes(&mut client_a, consumer_id, std::slice::from_ref(&note)).await;
    wait_for_tx(&mut client_a, consume_tx).await?;

    // B syncs until its reader surfaces the consumed note.
    let mut found = None;
    for _ in 0..15 {
        client_b.sync_state().await?;
        let mut reader = client_b.input_note_reader(consumer_id);
        while let Some(n) = reader.next().await? {
            if n.id() == Some(note.id()) {
                found = Some(n);
                break;
            }
        }
        if found.is_some() {
            break;
        }
        wait_for_blocks(&mut client_b, 1).await;
    }

    let found = found.context(
        "watched account's reader did not surface the consumed note via consumed_note_refs",
    )?;
    assert_eq!(
        found.details_commitment(),
        note.details_commitment(),
        "consumed public note should carry the full details fetched by id from the node",
    );
    assert_eq!(found.consumer_account(), Some(consumer_id));
    assert_eq!(found.id(), Some(note.id()));

    Ok(())
}

/// Verifies syncing and consuming notes with attachments, for both a public and a private note.
/// 1. Client 1 mints a public and a private P2ID note, each with an attachment, targeting client 2.
/// 2. Client 2 syncs and discovers both notes via `sync_notes`.
/// 3. The sync triggers a `get_notes_by_id` call that resolves the public note's body and the
///    private note's attachment content.
/// 4. Client 2 consumes both notes.
pub async fn test_sync_note_with_attachment(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client_1).await;

    // Create faucet in client 1
    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Private,
        &keystore_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // Create wallet in client 2
    let (wallet, ..) =
        insert_new_wallet(&mut client_2, AccountType::Private, &keystore_2, RPO_FALCON_SCHEME_ID)
            .await?;

    client_1.sync_state().await?;
    client_2.sync_state().await?;

    // Mint two P2ID notes carrying Word attachments: one public, one private.
    let public_attachments = NoteAttachments::new(vec![NoteAttachment::with_word(
        NoteAttachmentScheme::new(42)?,
        Word::from([1u32, 2, 3, 4]),
    )])?;
    let private_attachments = NoteAttachments::new(vec![NoteAttachment::with_word(
        NoteAttachmentScheme::new(43)?,
        Word::from([5u32, 6, 7, 8]),
    )])?;
    let asset = FungibleAsset::new(faucet_account.id(), MINT_AMOUNT)?;

    let public_note: Note = P2idNote::builder()
        .sender(faucet_account.id())
        .target(wallet.id())
        .asset(asset)
        .note_type(NoteType::Public)
        .attachments(public_attachments.into_vec())
        .generate_serial_number(client_1.rng())
        .build()?
        .into();
    let private_note: Note = P2idNote::builder()
        .sender(faucet_account.id())
        .target(wallet.id())
        .asset(asset)
        .note_type(NoteType::Private)
        .attachments(private_attachments.into_vec())
        .generate_serial_number(client_1.rng())
        .build()?
        .into();

    info!(public = %public_note.id(), private = %private_note.id(), "Minting P2ID notes with attachments");
    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![public_note.clone(), private_note.clone()])
        .build()?;
    execute_tx_and_sync(&mut client_1, faucet_account.id(), tx_request).await?;

    // A private note's details never appear on-chain, so client 2 must receive the details.
    client_2.add_note_tag(private_note.metadata().tag()).await?;
    client_2
        .import_notes(&[NoteFile::ExpectedNote {
            details: private_note.clone().into(),
            sync_hint: NoteSyncHint::new(0u32.into(), private_note.metadata().tag()),
        }])
        .await?;

    // Client 2 syncs and should discover both notes. sync_notes carries full metadata for both;
    // get_notes_by_id then resolves the public note body and the private note's attachment content.
    info!("Syncing client 2 to discover notes with attachments");
    client_2.sync_state().await?;

    // Both records must retain their attachment content and reconstruct to the same note ID as the
    // on-chain notes.
    let mut received_notes = vec![];
    for original in [&public_note, &private_note] {
        let record = client_2
            .get_input_note(original.id())
            .await?
            .with_context(|| format!("Note {} not found in client_2 after sync", original.id()))?;
        let received: InputNote = record.try_into()?;
        assert_eq!(
            received.note().attachments(),
            original.attachments(),
            "reconstructed note should retain the original attachment content",
        );
        assert_eq!(
            received.note().id(),
            original.id(),
            "reconstructed note must match the on-chain note ID",
        );
        received_notes.push(received.note().clone());
    }

    // Consume both notes — this fails if either note's attachments weren't resolved.
    info!("Consuming both notes with attachments in client 2");
    let tx_id = consume_notes(&mut client_2, wallet.id(), &received_notes).await;
    wait_for_tx(&mut client_2, tx_id).await?;

    assert_account_has_single_asset(&client_2, wallet.id(), faucet_account.id(), MINT_AMOUNT * 2)
        .await;

    Ok(())
}

use anyhow::{Context, Result};
use miden_client::account::AccountType;
use miden_client::asset::{Asset, AssetAmount, FungibleAsset};
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::note::standards::NoteSyncHint;
use miden_client::note::{Note, NoteDetails, NoteFile, NoteType, SwapNote};
use miden_client::store::NoteFilter;
use miden_client::testing::common::*;
use miden_client::transaction::{SwapTransactionData, TransactionRequestBuilder};
use tracing::info;

use crate::tests::config::ClientConfig;

// SWAP FULLY ONCHAIN
// ================================================================================================

pub async fn test_swap_fully_onchain(client_config: ClientConfig) -> Result<()> {
    const OFFERED_ASSET_AMOUNT: u64 = 1;
    const REQUESTED_ASSET_AMOUNT: u64 = 25;
    let (mut client1, authenticator_1) = client_config.clone().into_client().await?;
    wait_for_node(&mut client1).await;
    let (mut client2, authenticator_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    client1.sync_state().await?;
    client2.sync_state().await?;

    // Create Client 1's basic wallet (We'll call it accountA)
    let (account_a, ..) = insert_new_wallet(
        &mut client1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    // Create Client 2's basic wallet (We'll call it accountB)
    let (account_b, ..) = insert_new_wallet(
        &mut client2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // Create client with faucets BTC faucet (note: it's not real BTC)
    let (btc_faucet_account, _) = insert_new_fungible_faucet(
        &mut client1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // Create client with faucets ETH faucet (note: it's not real ETH)
    let (eth_faucet_account, _) = insert_new_fungible_faucet(
        &mut client2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // mint 1000 BTC for accountA
    info!(account_id = %account_a.id(), faucet_id = %btc_faucet_account.id(), "Minting 1000 BTC for account A");

    let tx_id =
        mint_and_consume(&mut client1, account_a.id(), btc_faucet_account.id(), NoteType::Public)
            .await;
    wait_for_tx(&mut client1, tx_id).await?;

    // mint 1000 ETH for accountB
    info!(account_id = %account_b.id(), faucet_id = %eth_faucet_account.id(), "Minting 1000 ETH for account B");

    let tx_id =
        mint_and_consume(&mut client2, account_b.id(), eth_faucet_account.id(), NoteType::Public)
            .await;
    wait_for_tx(&mut client2, tx_id).await?;

    // Create ONCHAIN swap note (clientA offers 1 BTC in exchange of 25 ETH)
    // check that account now has 1 less BTC
    let offered_asset = FungibleAsset::new(btc_faucet_account.id(), OFFERED_ASSET_AMOUNT)?;
    let requested_asset = FungibleAsset::new(eth_faucet_account.id(), REQUESTED_ASSET_AMOUNT)?;
    info!(account_id = %account_a.id(), offered_amount = OFFERED_ASSET_AMOUNT, requested_amount = REQUESTED_ASSET_AMOUNT, "Creating swap note");

    info!("Executing SWAP transaction");
    let tx_request = TransactionRequestBuilder::new().build_swap(
        &SwapTransactionData::new(
            account_a.id(),
            Asset::Fungible(offered_asset),
            Asset::Fungible(requested_asset),
        ),
        NoteType::Public,
        NoteType::Private,
        client1.rng(),
    )?;

    let expected_output_notes: Vec<Note> = tx_request.expected_output_own_notes();
    let expected_payback_note_details: Vec<NoteDetails> =
        tx_request.expected_future_notes().cloned().map(|(n, _)| n).collect();
    assert_eq!(expected_output_notes.len(), 1);
    assert_eq!(expected_payback_note_details.len(), 1);

    execute_tx_and_sync(&mut client1, account_a.id(), tx_request).await?;

    let swap_note_tag = SwapNote::build_tag(
        NoteType::Public,
        &Asset::Fungible(offered_asset),
        &Asset::Fungible(requested_asset),
    );

    // add swap note's tag to client2
    // we could technically avoid this step, but for the first iteration of swap notes we'll
    // require to manually add tags
    info!(tag = %swap_note_tag, "Adding swap note tag to client 2");
    client2.add_note_tag(swap_note_tag).await?;

    // sync on client 2, we should get the swap note
    // consume swap note with accountB, and check that the vault changed appropriately
    client2.sync_state().await?;
    info!(note_id = %expected_output_notes[0].id(), account_id = %account_b.id(), "Consuming swap note on client 2");

    let note = client2
        .get_input_note(expected_output_notes[0].id())
        .await?
        .unwrap()
        .try_into()?;
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note])?;
    execute_tx_and_sync(&mut client2, account_b.id(), tx_request).await?;

    // sync on client 1, we should get the missing payback note details.
    // try consuming the received note with accountA, it should now have 25 ETH
    client1.sync_state().await?;
    let payback_commitment = expected_payback_note_details[0].commitment();
    info!(payback_commitment = %payback_commitment.to_hex(), account_id = %account_a.id(), "Consuming swap payback note on client 1");

    let note = client1
        .get_input_notes(NoteFilter::DetailsCommitments(vec![payback_commitment]))
        .await?
        .pop()
        .expect("payback note should be present after sync")
        .try_into()?;
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note])?;
    execute_tx_and_sync(&mut client1, account_a.id(), tx_request).await?;

    // At the end we should end up with
    //
    // - accountA: 999 BTC, 25 ETH
    // - accountB: 1 BTC, 975 ETH

    let account_a_reader = client1.account_reader(account_a.id());
    let account_a_btc = account_a_reader.get_balance(btc_faucet_account.id()).await?;
    let account_a_eth = account_a_reader.get_balance(eth_faucet_account.id()).await?;

    assert_eq!(account_a_btc, AssetAmount::new(999).unwrap());
    assert_eq!(account_a_eth, AssetAmount::new(25).unwrap());

    let account_b_reader = client2.account_reader(account_b.id());
    let account_b_btc = account_b_reader.get_balance(btc_faucet_account.id()).await?;
    let account_b_eth = account_b_reader.get_balance(eth_faucet_account.id()).await?;

    assert_eq!(account_b_btc, AssetAmount::new(1).unwrap());
    assert_eq!(account_b_eth, AssetAmount::new(975).unwrap());

    Ok(())
}

/// Same shape as `test_swap_fully_onchain` but with a public payback note. Exercises the
/// deterministic payback-recipient derivation: the consumer materializes the public payback
/// without any off-band advice, the node indexes it, and the original sender retrieves and
/// consumes it after a regular sync.
pub async fn test_swap_public_payback(client_config: ClientConfig) -> Result<()> {
    const OFFERED_ASSET_AMOUNT: u64 = 1;
    const REQUESTED_ASSET_AMOUNT: u64 = 25;
    let (mut client1, authenticator_1) = client_config.clone().into_client().await?;
    wait_for_node(&mut client1).await;
    let (mut client2, authenticator_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    client1.sync_state().await?;
    client2.sync_state().await?;

    let (account_a, ..) = insert_new_wallet(
        &mut client1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (account_b, ..) = insert_new_wallet(
        &mut client2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (btc_faucet_account, _) = insert_new_fungible_faucet(
        &mut client1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (eth_faucet_account, _) = insert_new_fungible_faucet(
        &mut client2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    info!(account_id = %account_a.id(), faucet_id = %btc_faucet_account.id(), "Minting 1000 BTC for account A");
    let tx_id =
        mint_and_consume(&mut client1, account_a.id(), btc_faucet_account.id(), NoteType::Public)
            .await;
    wait_for_tx(&mut client1, tx_id).await?;

    info!(account_id = %account_b.id(), faucet_id = %eth_faucet_account.id(), "Minting 1000 ETH for account B");
    let tx_id =
        mint_and_consume(&mut client2, account_b.id(), eth_faucet_account.id(), NoteType::Public)
            .await;
    wait_for_tx(&mut client2, tx_id).await?;

    let offered_asset = FungibleAsset::new(btc_faucet_account.id(), OFFERED_ASSET_AMOUNT)?;
    let requested_asset = FungibleAsset::new(eth_faucet_account.id(), REQUESTED_ASSET_AMOUNT)?;
    info!(account_id = %account_a.id(), "Creating swap note with public payback");

    let tx_request = TransactionRequestBuilder::new().build_swap(
        &SwapTransactionData::new(
            account_a.id(),
            Asset::Fungible(offered_asset),
            Asset::Fungible(requested_asset),
        ),
        NoteType::Public,
        NoteType::Public,
        client1.rng(),
    )?;

    let expected_output_notes: Vec<Note> = tx_request.expected_output_own_notes();
    let expected_payback_note_details: Vec<NoteDetails> =
        tx_request.expected_future_notes().cloned().map(|(n, _)| n).collect();
    assert_eq!(expected_output_notes.len(), 1);
    assert_eq!(expected_payback_note_details.len(), 1);

    execute_tx_and_sync(&mut client1, account_a.id(), tx_request).await?;

    let swap_note_tag = SwapNote::build_tag(
        NoteType::Public,
        &Asset::Fungible(offered_asset),
        &Asset::Fungible(requested_asset),
    );
    info!(tag = %swap_note_tag, "Adding swap note tag to client 2");
    client2.add_note_tag(swap_note_tag).await?;

    client2.sync_state().await?;
    info!(note_id = %expected_output_notes[0].id(), account_id = %account_b.id(), "Consuming swap note on client 2");

    let note = client2
        .get_input_note(expected_output_notes[0].id())
        .await?
        .unwrap()
        .try_into()?;
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note])?;
    execute_tx_and_sync(&mut client2, account_b.id(), tx_request).await?;

    client1.sync_state().await?;
    let payback_commitment = expected_payback_note_details[0].commitment();
    info!(note = %payback_commitment.to_hex(), account_id = %account_a.id(), "Consuming public payback note on client 1");

    let payback_record = client1
        .get_input_notes(NoteFilter::DetailsCommitments(vec![payback_commitment]))
        .await?
        .pop()
        .with_context(|| format!("Payback note {} not found", payback_commitment.to_hex()))?;

    // The node must have committed the payback as a public note.
    assert_eq!(
        payback_record
            .metadata()
            .context("payback note should have metadata")?
            .note_type(),
        NoteType::Public
    );

    let note: Note = payback_record.try_into()?;
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note])?;
    execute_tx_and_sync(&mut client1, account_a.id(), tx_request).await?;

    let account_a_reader = client1.account_reader(account_a.id());
    let account_a_btc = account_a_reader.get_balance(btc_faucet_account.id()).await?;
    let account_a_eth = account_a_reader.get_balance(eth_faucet_account.id()).await?;
    assert_eq!(account_a_btc, AssetAmount::new(999).unwrap());
    assert_eq!(account_a_eth, AssetAmount::new(25).unwrap());

    let account_b_reader = client2.account_reader(account_b.id());
    let account_b_btc = account_b_reader.get_balance(btc_faucet_account.id()).await?;
    let account_b_eth = account_b_reader.get_balance(eth_faucet_account.id()).await?;
    assert_eq!(account_b_btc, AssetAmount::new(1).unwrap());
    assert_eq!(account_b_eth, AssetAmount::new(975).unwrap());

    Ok(())
}

pub async fn test_swap_private(client_config: ClientConfig) -> Result<()> {
    const OFFERED_ASSET_AMOUNT: u64 = 1;
    const REQUESTED_ASSET_AMOUNT: u64 = 25;
    let (mut client1, authenticator_1) = client_config.clone().into_client().await?;
    wait_for_node(&mut client1).await;
    let (mut client2, authenticator_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    client1.sync_state().await?;
    client2.sync_state().await?;

    // Create Client 1's basic wallet (We'll call it accountA)
    let (account_a, ..) = insert_new_wallet(
        &mut client1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    // Create Client 2's basic wallet (We'll call it accountB)
    let (account_b, ..) = insert_new_wallet(
        &mut client2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // Create client with faucets BTC faucet (note: it's not real BTC)
    let (btc_faucet_account, _) = insert_new_fungible_faucet(
        &mut client1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    // Create client with faucets ETH faucet (note: it's not real ETH)
    let (eth_faucet_account, _) = insert_new_fungible_faucet(
        &mut client2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // mint 1000 BTC for accountA
    info!(account_id = %account_a.id(), faucet_id = %btc_faucet_account.id(), "Minting 1000 BTC for account A");
    let tx_id =
        mint_and_consume(&mut client1, account_a.id(), btc_faucet_account.id(), NoteType::Public)
            .await;
    wait_for_tx(&mut client1, tx_id).await?;

    // mint 1000 ETH for accountB
    info!(account_id = %account_b.id(), faucet_id = %eth_faucet_account.id(), "Minting 1000 ETH for account B");
    let tx_id =
        mint_and_consume(&mut client2, account_b.id(), eth_faucet_account.id(), NoteType::Public)
            .await;
    wait_for_tx(&mut client2, tx_id).await?;

    // Create ONCHAIN swap note (clientA offers 1 BTC in exchange of 25 ETH)
    // check that account now has 1 less BTC
    let offered_asset = FungibleAsset::new(btc_faucet_account.id(), OFFERED_ASSET_AMOUNT)?;
    let requested_asset = FungibleAsset::new(eth_faucet_account.id(), REQUESTED_ASSET_AMOUNT)?;
    info!(account_id = %account_a.id(), offered_amount = OFFERED_ASSET_AMOUNT, requested_amount = REQUESTED_ASSET_AMOUNT, "Creating swap note");

    info!("Executing SWAP transaction");
    let tx_request = TransactionRequestBuilder::new().build_swap(
        &SwapTransactionData::new(
            account_a.id(),
            Asset::Fungible(offered_asset),
            Asset::Fungible(requested_asset),
        ),
        NoteType::Private,
        NoteType::Private,
        client1.rng(),
    )?;

    let expected_output_notes: Vec<Note> = tx_request.expected_output_own_notes();
    let expected_payback_note_details =
        tx_request.expected_future_notes().cloned().map(|(n, _)| n).collect::<Vec<_>>();
    assert_eq!(expected_output_notes.len(), 1);
    assert_eq!(expected_payback_note_details.len(), 1);

    execute_tx_and_sync(&mut client1, account_a.id(), tx_request).await?;

    // Export note from client 1 to client 2
    let output_note = client1
        .get_output_note(expected_output_notes[0].id())
        .await?
        .with_context(|| format!("Output note {} not found", expected_output_notes[0].id()))?;

    let tag = SwapNote::build_tag(
        NoteType::Private,
        &Asset::Fungible(offered_asset),
        &Asset::Fungible(requested_asset),
    );
    client2.add_note_tag(tag).await?;
    client2
        .import_notes(&[NoteFile::ExpectedNote {
            details: output_note.try_into()?,
            sync_hint: NoteSyncHint::new(client1.get_sync_height().await?, tag),
        }])
        .await?;

    // Sync so we get the inclusion proof info
    client2.sync_state().await?;

    // consume swap note with accountB, and check that the vault changed appropriately
    info!(note_id = %expected_output_notes[0].id(), account_id = %account_b.id(), "Consuming swap note on client 2");

    let note = client2
        .get_input_note(expected_output_notes[0].id())
        .await?
        .unwrap()
        .try_into()?;
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note])?;
    execute_tx_and_sync(&mut client2, account_b.id(), tx_request).await?;

    // sync on client 1, we should get the missing payback note details.
    // try consuming the received note with accountA, it should now have 25 ETH
    client1.sync_state().await?;
    let payback_commitment = expected_payback_note_details[0].commitment();
    info!(payback_commitment = %payback_commitment.to_hex(), account_id = %account_a.id(), "Consuming swap payback note on client 1");

    let note = client1
        .get_input_notes(NoteFilter::DetailsCommitments(vec![payback_commitment]))
        .await?
        .pop()
        .expect("payback note should be present after sync")
        .try_into()?;
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note])?;
    execute_tx_and_sync(&mut client1, account_a.id(), tx_request).await?;

    // At the end we should end up with
    //
    // - accountA: 999 BTC, 25 ETH
    // - accountB: 1 BTC, 975 ETH

    let account_a_reader = client1.account_reader(account_a.id());
    let account_a_btc = account_a_reader.get_balance(btc_faucet_account.id()).await?;
    let account_a_eth = account_a_reader.get_balance(eth_faucet_account.id()).await?;

    assert_eq!(account_a_btc, AssetAmount::new(999).unwrap());
    assert_eq!(account_a_eth, AssetAmount::new(25).unwrap());

    let account_b_reader = client2.account_reader(account_b.id());
    let account_b_btc = account_b_reader.get_balance(btc_faucet_account.id()).await?;
    let account_b_eth = account_b_reader.get_balance(eth_faucet_account.id()).await?;

    assert_eq!(account_b_btc, AssetAmount::new(1).unwrap());
    assert_eq!(account_b_eth, AssetAmount::new(975).unwrap());

    Ok(())
}

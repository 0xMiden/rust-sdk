use anyhow::{Context, Result};
use miden_client::account::AccountType;
use miden_client::asset::FungibleAsset;
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::note::{NoteFile, NoteType};
use miden_client::store::{InputNoteRecord, NoteFilter};
use miden_client::sync::NoteTagSource;
use miden_client::testing::common::*;
use miden_client::transaction::{InputNote, PaymentNoteDescription, TransactionRequestBuilder};

use crate::tests::config::ClientConfig;

// HELPERS
// ================================================================================================

/// Asserts that the client tracks no `Note`-sourced tags.
async fn assert_no_note_sourced_tags(client: &TestClient, context: &str) -> Result<()> {
    let tags = client.get_note_tags().await?;
    assert!(
        tags.iter().all(|tag| !matches!(tag.source, NoteTagSource::Note(_))),
        "unexpected Note-sourced tags {context}: {tags:?}"
    );
    Ok(())
}

// TESTS
// ================================================================================================

/// Output notes must not register tags: commitment and inclusion proof arrive via
/// account-matched transaction sync, and the recipient still gets the note via its account tag.
pub async fn test_output_notes_do_not_register_tags(client_config: ClientConfig) -> Result<()> {
    // Client 1 runs the faucet; client 2 tracks the recipient wallet, so from client 1's
    // perspective the minted note goes to an external account.
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .with_note_transport_endpoint(None)
        .into_client()
        .await?;
    wait_for_node(&mut client_2).await;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Private,
        &keystore_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (basic_wallet, ..) =
        insert_new_wallet(&mut client_2, AccountType::Private, &keystore_2, RPO_FALCON_SCHEME_ID)
            .await?;
    client_1.sync_state().await?;
    client_2.sync_state().await?;

    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        FungibleAsset::new(faucet_account.id(), MINT_AMOUNT)?,
        basic_wallet.id(),
        NoteType::Public,
        client_1.rng(),
    )?;
    let note = tx_request
        .expected_output_own_notes()
        .pop()
        .context("expected an output note in the mint transaction")?;

    let tx_id = Box::pin(client_1.submit_new_transaction(faucet_account.id(), tx_request)).await?;

    // Applying the transaction must not have registered a tag for the output note.
    assert_no_note_sourced_tags(&client_1, "after applying a mint to an external account").await?;

    wait_for_tx(&mut client_1, tx_id).await?;

    // The output note must be committed with its inclusion proof, obtained purely via
    // account-matched transaction sync.
    let output_note = client_1
        .get_output_notes(NoteFilter::Unique(note.id()))
        .await?
        .pop()
        .context("minted output note should be tracked")?;
    assert!(output_note.is_committed(), "output note should be committed after sync");
    assert!(
        output_note.inclusion_proof().is_some(),
        "committed output note should carry an inclusion proof from transaction sync"
    );
    assert_no_note_sourced_tags(&client_1, "after syncing the mint transaction").await?;

    // The external recipient still receives the note through its account tag and can consume it.
    client_2.sync_state().await?;
    let received_record = client_2
        .get_input_note(note.id())
        .await?
        .context("recipient should have received the minted note")?;
    // Guard against the unauthenticated-input fallback: the conversion below succeeds even
    // without an inclusion proof, so committedness must be asserted explicitly.
    assert!(received_record.is_committed(), "received note should be committed");
    let received_note: InputNote = received_record.try_into()?;
    let tx_id =
        consume_notes(&mut client_2, basic_wallet.id(), &[received_note.note().clone()]).await;
    wait_for_tx(&mut client_2, tx_id).await?;
    assert_account_has_single_asset(&client_2, basic_wallet.id(), faucet_account.id(), MINT_AMOUNT)
        .await;

    Ok(())
}

/// Expected input notes register exactly one tag and it is cleaned up on commit — covered for
/// a self-directed transfer and for an expected note imported by details.
pub async fn test_input_note_tag_lifecycle(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, keystore_1) = client_config.clone().into_client().await?;
    let (mut client_2, keystore_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .with_note_transport_endpoint(None)
        .into_client()
        .await?;
    wait_for_node(&mut client_1).await;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Private,
        &keystore_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (wallet_a, ..) =
        insert_new_wallet(&mut client_1, AccountType::Private, &keystore_1, RPO_FALCON_SCHEME_ID)
            .await?;
    let (wallet_b, ..) =
        insert_new_wallet(&mut client_1, AccountType::Private, &keystore_1, RPO_FALCON_SCHEME_ID)
            .await?;
    let (wallet_c, ..) =
        insert_new_wallet(&mut client_2, AccountType::Private, &keystore_2, RPO_FALCON_SCHEME_ID)
            .await?;
    client_1.sync_state().await?;
    client_2.sync_state().await?;

    // Fund wallet A.
    let tx_id =
        mint_and_consume(&mut client_1, wallet_a.id(), faucet_account.id(), NoteType::Private)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // Self-directed transfer: sender and recipient are both tracked by client 1, so the note is
    // registered as an expected input note with a tag.
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(
            vec![FungibleAsset::new(faucet_account.id(), TRANSFER_AMOUNT)?.into()],
            wallet_a.id(),
            wallet_b.id(),
        ),
        NoteType::Private,
        client_1.rng(),
    )?;
    let note = tx_request
        .expected_output_own_notes()
        .pop()
        .context("expected an output note in the transfer transaction")?;
    let tx_id = Box::pin(client_1.submit_new_transaction(wallet_a.id(), tx_request)).await?;

    let matching_tags = client_1
        .get_note_tags()
        .await?
        .into_iter()
        .filter(|tag| tag.source == NoteTagSource::Note(note.details_commitment()))
        .count();
    assert_eq!(
        matching_tags, 1,
        "a self-directed expected input note should register exactly one tag"
    );

    wait_for_tx(&mut client_1, tx_id).await?;

    // Once the note commits, the tag is cleaned up and both note records carry their state.
    assert_no_note_sourced_tags(&client_1, "after the self-directed note committed").await?;
    let output_note = client_1
        .get_output_notes(NoteFilter::Unique(note.id()))
        .await?
        .pop()
        .context("self-directed output note should be tracked")?;
    assert!(output_note.is_committed());
    assert!(output_note.inclusion_proof().is_some());
    let received_record = client_1
        .get_input_note(note.id())
        .await?
        .context("self-directed note should be tracked as an input note")?;
    assert!(received_record.is_committed(), "self-directed input note should be committed");
    let received_note: InputNote = received_record.try_into()?;
    let tx_id = consume_notes(&mut client_1, wallet_b.id(), &[received_note.note().clone()]).await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // Importing an expected note by details (before it is committed on chain) registers a tag
    // and cleans it up once the note commits.
    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        FungibleAsset::new(faucet_account.id(), MINT_AMOUNT)?,
        wallet_c.id(),
        NoteType::Private,
        client_1.rng(),
    )?;
    let expected_note = tx_request
        .expected_output_own_notes()
        .pop()
        .context("expected an output note in the mint transaction")?;
    let expected_note_id = expected_note.id();
    let note_record: InputNoteRecord = expected_note.into();
    let note_tag = note_record.metadata().context("expected note should have metadata")?.tag();

    client_2
        .import_notes(&[NoteFile::NoteDetails {
            details: note_record.clone().into(),
            after_block_num: client_1.get_sync_height().await?,
            tag: Some(note_tag),
        }])
        .await?;
    assert_eq!(
        client_2
            .get_note_tags()
            .await?
            .into_iter()
            .filter(|tag| tag.source == NoteTagSource::Note(note_record.details_commitment()))
            .count(),
        1,
        "importing an expected note should register its tag"
    );

    let tx_id = Box::pin(client_1.submit_new_transaction(faucet_account.id(), tx_request)).await?;
    wait_for_tx(&mut client_1, tx_id).await?;

    client_2.sync_state().await?;
    assert_no_note_sourced_tags(&client_2, "after the imported note committed").await?;
    let received_record = client_2
        .get_input_note(expected_note_id)
        .await?
        .context("imported note should be committed for the recipient")?;
    assert!(received_record.is_committed(), "imported note should be committed");
    let received_note: InputNote = received_record.try_into()?;
    let tx_id = consume_notes(&mut client_2, wallet_c.id(), &[received_note.note().clone()]).await;
    wait_for_tx(&mut client_2, tx_id).await?;

    Ok(())
}

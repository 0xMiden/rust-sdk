use anyhow::Result;
use miden_client::account::component::BasicWallet;
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountId,
    AccountType,
};
use miden_client::assembly::CodeBuilder;
use miden_client::asset::{Asset, AssetAmount, FungibleAsset};
use miden_client::auth::{AuthSchemeId, NoAuth, TransactionAuthenticator};
use miden_client::crypto::FeltRng;
use miden_client::note::{
    Note,
    NoteAssets,
    NoteDetails,
    NoteFile,
    NoteRecipient,
    NoteScript,
    NoteStorage,
    NoteTag,
    NoteType,
    P2idNoteStorage,
    PartialNoteMetadata,
};
use miden_client::store::{InputNoteState, TransactionFilter};
use miden_client::testing::common::*;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, ClientRng, Word};
use rand::Rng;
use tracing::info;

use crate::tests::config::ClientConfig;

// PASS-THROUGH TRANSACTIONS (change sender from Alice -> Pass-through account)
// ================================================================================================

pub async fn test_pass_through(client_config: ClientConfig) -> Result<()> {
    const ASSET_AMOUNT: u64 = 1;
    let (mut client, authenticator_1) = client_config.clone().into_client().await?;

    // Workaround to show that importing the note into another client works
    let (mut client_2, authenticator_2) =
        client_config.clone().with_fresh_store().into_client().await?;

    wait_for_node(&mut client).await;
    client.sync_state().await?;
    client_2.sync_state().await?;

    // Create Client basic wallet (We'll call it accountA)
    let (sender, ..) = insert_new_wallet(
        &mut client,
        AccountType::Private,
        &authenticator_1,
        AuthSchemeId::Falcon512Poseidon2,
    )
    .await?;
    let (target, ..) = insert_new_wallet(
        &mut client_2,
        AccountType::Private,
        &authenticator_2,
        AuthSchemeId::Falcon512Poseidon2,
    )
    .await?;

    // `NoAuth` pays the transaction fee out of the account's own vault like any other auth
    // component, so on a fee-charging chain the pass-through account has to be funded before it can
    // consume anything. Deploying it fee-free would be wrong, though: `NoAuth` only bumps the nonce
    // when the account state changed, and an empty transaction changes nothing.
    let charges_fees = client.chain_charges_fees().await?;
    let pass_through_account = create_pass_through_account(&mut client).await?;
    if charges_fees {
        client.deploy_account(pass_through_account.id()).await?;
    }

    // Create client with faucets BTC faucet
    let (btc_faucet_account, ..) = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &authenticator_1,
        AuthSchemeId::Falcon512Poseidon2,
    )
    .await?;

    // mint 1000 BTC for accountA
    info!(account_id = %sender.id(), faucet_id = %btc_faucet_account.id(), "Minting 1000 BTC for sender");

    let tx_id =
        mint_and_consume(&mut client, sender.id(), btc_faucet_account.id(), NoteType::Public).await;
    wait_for_tx(&mut client, tx_id).await?;

    // Create a note that we will send to a pass-through account
    info!(sender_id = %sender.id(), target_id = %target.id(), "Creating pass-through note");
    let asset = FungibleAsset::new(btc_faucet_account.id(), ASSET_AMOUNT)?;

    let (pass_through_note_1, pass_through_note_details_1) =
        create_pass_through_note(sender.id(), target.id(), asset.into(), client.rng())?;

    let (pass_through_note_2, pass_through_note_details_2) =
        create_pass_through_note(sender.id(), target.id(), asset.into(), client.rng())?;

    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![pass_through_note_1.clone(), pass_through_note_2.clone()])
        .build()?;

    execute_tx_and_sync(&mut client, sender.id(), tx_request).await?;

    info!(note_id = %pass_through_note_1.id(), pass_through_account = %pass_through_account.id(), "Consuming pass-through note");

    client
        .import_notes(&[
            NoteFile::NoteId(pass_through_note_1.id()),
            NoteFile::NoteId(pass_through_note_2.id()),
        ])
        .await?;
    client.sync_state().await?;
    let input_note_record = client.get_input_note(pass_through_note_1.id()).await?.unwrap();
    assert!(matches!(input_note_record.state(), InputNoteState::Committed { .. }));
    let input_note_record = client.get_input_note(pass_through_note_2.id()).await?.unwrap();
    assert!(matches!(input_note_record.state(), InputNoteState::Committed { .. }));

    let tx_request = TransactionRequestBuilder::new()
        .expected_output_recipients(vec![pass_through_note_details_1.recipient().clone()])
        .build_consume_notes(vec![pass_through_note_1])
        .unwrap();

    let tx_id = client
        .submit_new_transaction(pass_through_account.id(), tx_request.clone())
        .await?;

    wait_for_tx(&mut client, tx_id).await?;

    let tx_record = client
        .get_transactions(TransactionFilter::Ids(vec![tx_id]))
        .await?
        .pop()
        .unwrap();

    assert_eq!(
        tx_record.details.output_notes.get_note(0).metadata().sender(),
        pass_through_account.id()
    );

    // Storing commitment to check later that (final_acc.commitment == initial_acc.commitment)
    let commitment_before_second_tx = client
        .account_reader(pass_through_account.id())
        .commitment()
        .await
        .expect("pass-through account should exist");
    // Held separately because a fee-charging chain moves the overall commitment, leaving storage
    // as the part the pass-through still has to leave alone.
    let storage_commitment_before_second_tx = client
        .account_reader(pass_through_account.id())
        .storage_commitment()
        .await
        .expect("pass-through account should exist");

    // now try another transaction against the pass-through account
    let tx_request = TransactionRequestBuilder::new()
        .expected_output_recipients(vec![pass_through_note_details_2.recipient().clone()])
        .build_consume_notes(vec![pass_through_note_2])
        .unwrap();

    let tx_id = client
        .submit_new_transaction(pass_through_account.id(), tx_request.clone())
        .await?;

    wait_for_tx(&mut client, tx_id).await?;

    let tx_record = client
        .get_transactions(TransactionFilter::Ids(vec![tx_id]))
        .await?
        .pop()
        .unwrap();

    assert_eq!(
        tx_record.details.output_notes.get_note(0).metadata().sender(),
        pass_through_account.id()
    );

    let commitment_after_second_tx = client
        .account_reader(pass_through_account.id())
        .commitment()
        .await
        .expect("pass-through account should exist");

    if charges_fees {
        // Paying the fee withdraws from the vault, so the account commitment necessarily moves.
        // What pass-through actually promises is asserted directly instead: the forwarded asset is
        // not retained, and storage is untouched.
        let reader = client.account_reader(pass_through_account.id());
        let retained = reader.get_balance(btc_faucet_account.id()).await?;
        assert_eq!(
            retained,
            AssetAmount::ZERO,
            "pass-through account should not retain the forwarded asset"
        );
        assert_eq!(
            reader.storage_commitment().await?,
            storage_commitment_before_second_tx,
            "a pass-through transaction should not touch account storage"
        );
    } else {
        assert_eq!(
            commitment_after_second_tx, commitment_before_second_tx,
            "pass-through transaction should not change account commitment"
        );
    }

    Ok(())
}

// HELPERS
// ================================================================================================

async fn create_pass_through_account<AUTH: TransactionAuthenticator>(
    client: &mut Client<AUTH>,
) -> Result<Account> {
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    // The pass-through consumption must not change the account commitment on a fee-free chain: the
    // note moves the asset straight back out, and `NoAuth` only bumps the nonce when the account
    // state differs at the end of the transaction. Where a fee is charged, paying it withdraws
    // from the vault, so only storage stays put.
    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Private)
        .with_component(NoAuth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();

    client.add_account(&account, false).await?;
    Ok(account)
}

fn get_pass_through_note_script() -> NoteScript {
    let note_script_code = include_str!("../asm/PASS_THROUGH.masm");

    CodeBuilder::new().compile_note_script(note_script_code).unwrap()
}

// Creates a note eventually meant for the target account.
// First, the note is processed by the pass-through account.
// The output note script guarantees the output of the processing is `target`.
fn create_pass_through_note(
    sender: AccountId,
    target: AccountId,
    asset: Asset,
    rng: &mut ClientRng,
) -> Result<(Note, NoteDetails)> {
    let note_script = get_pass_through_note_script();

    let asset_key: Word = asset.to_id_word();
    let asset_value: Word = asset.to_value_word();

    let target_recipient = P2idNoteStorage::new(target).into_recipient(rng.draw_word());

    let inputs = NoteStorage::new(vec![
        asset_key[0],
        asset_key[1],
        asset_key[2],
        asset_key[3],
        asset_value[0],
        asset_value[1],
        asset_value[2],
        asset_value[3],
        target_recipient.digest()[0],
        target_recipient.digest()[1],
        target_recipient.digest()[2],
        target_recipient.digest()[3],
        NoteType::Public.into(),
        NoteTag::with_account_target(target).into(),
    ])?;

    let serial_num = rng.draw_word();
    let pass_through_recipient = NoteRecipient::new(serial_num, note_script, inputs);

    let metadata = PartialNoteMetadata::new(sender, NoteType::Public)
        .with_tag(NoteTag::with_account_target(target));
    let note = Note::new(NoteAssets::new(vec![asset])?, metadata, pass_through_recipient);

    let pass_through_note_details =
        NoteDetails::new(NoteAssets::new(vec![asset])?, target_recipient);
    Ok((note, pass_through_note_details))
}

use anyhow::{Context, Result};
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
use miden_client::block::BlockNumber;
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
use miden_protocol::MAX_TX_EXECUTION_CYCLES;
use miden_protocol::transaction::TransactionFee;
use rand::Rng;
use tracing::info;

use crate::ClientConfig;

// PASS-THROUGH TRANSACTIONS (change sender from Alice -> Pass-through account)
// ================================================================================================

pub async fn test_pass_through(client_config: ClientConfig) -> Result<()> {
    const ASSET_AMOUNT: u64 = 1;
    let (mut client, authenticator_1) = client_config.clone().into_client().await?;

    // Workaround to show that importing the note into another client works
    let (mut client_2, authenticator_2) = client_config.clone().into_client().await?;

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

    let pass_through_account = create_pass_through_account(&mut client).await?;

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

    // On a fee-charging chain each note also carries exactly the fee its consumption pays, so
    // `pay_fee` withdraws precisely what the note deposited and the vault ends the transaction
    // as it started.
    let fee_asset = pass_through_fee_asset(
        &mut client,
        sender.id(),
        target.id(),
        asset.into(),
        pass_through_account.id(),
    )
    .await?;

    let (pass_through_note_1, pass_through_note_details_1) = create_pass_through_note(
        sender.id(),
        target.id(),
        asset.into(),
        fee_asset.map(Into::into),
        client.rng(),
    )?;

    let (pass_through_note_2, pass_through_note_details_2) = create_pass_through_note(
        sender.id(),
        target.id(),
        asset.into(),
        fee_asset.map(Into::into),
        client.rng(),
    )?;

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

    // The first consumption is also the account's on-chain creation, and `NoAuth` always bumps a
    // new account's nonce, so the commitment moves here. The vault must still come out empty:
    // the forwarded asset moved on and the fee asset was spent in full on the fee.
    let retained_btc = client
        .account_reader(pass_through_account.id())
        .get_balance(btc_faucet_account.id())
        .await?;
    assert_eq!(
        retained_btc,
        AssetAmount::ZERO,
        "pass-through account should not retain the forwarded asset"
    );
    if let Some(fee_asset) = fee_asset {
        let retained_fee = client
            .account_reader(pass_through_account.id())
            .get_balance(fee_asset.faucet_id())
            .await?;
        assert_eq!(
            retained_fee,
            AssetAmount::ZERO,
            "the note's fee asset should be spent in full on the fee"
        );
    }

    let commitment_before_second_tx = client
        .account_reader(pass_through_account.id())
        .commitment()
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

    assert_eq!(
        commitment_after_second_tx, commitment_before_second_tx,
        "pass-through transaction should not change account commitment"
    );

    Ok(())
}

// HELPERS
// ================================================================================================

async fn create_pass_through_account<AUTH: TransactionAuthenticator>(
    client: &mut Client<AUTH>,
) -> Result<Account> {
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    // The pass-through consumption must not change the account commitment: the note moves the
    // asset straight back out, and `NoAuth` only bumps the nonce when the account state differs
    // at the end of the transaction.
    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Private)
        .with_component(NoAuth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();

    client.add_account(&account, false).await?;
    Ok(account)
}

/// Returns the native fee asset each pass-through note must carry for its consumption to settle
/// its own fee, or `None` on a fee-free chain.
async fn pass_through_fee_asset<AUTH: TransactionAuthenticator + Sync + 'static>(
    client: &mut Client<AUTH>,
    sender: AccountId,
    target: AccountId,
    asset: Asset,
    pass_through_account_id: AccountId,
) -> Result<Option<FungibleAsset>> {
    let (genesis, _) = client
        .get_block_header_by_num(BlockNumber::GENESIS)
        .await?
        .context("the genesis block header is not in the client's store")?;
    let fee_parameters = genesis.fee_parameters();
    if fee_parameters.verification_base_fee() == 0 {
        return Ok(None);
    }

    let worst_case_fee =
        TransactionFee::new(MAX_TX_EXECUTION_CYCLES)?.compute_fee(fee_parameters)?;
    let funding = FungibleAsset::new(fee_parameters.fee_faucet_id(), worst_case_fee.as_u64())?;

    let (probe_note, probe_details) =
        create_pass_through_note(sender, target, asset, Some(funding.into()), client.rng())?;
    let request = TransactionRequestBuilder::new()
        .expected_output_recipients(vec![probe_details.recipient().clone()])
        .build_consume_notes(vec![probe_note])?;
    let fee = client
        .execute_transaction(pass_through_account_id, request)
        .await?
        .executed_transaction()
        .compute_fee();

    Ok(Some(FungibleAsset::new(fee_parameters.fee_faucet_id(), fee.as_u64())?))
}

fn get_pass_through_note_script() -> NoteScript {
    let note_script_code = include_str!("../asm/PASS_THROUGH.masm");

    CodeBuilder::new().compile_note_script(note_script_code).unwrap()
}

// Creates a note eventually meant for the target account.
// First, the note is processed by the pass-through account.
// The output note script guarantees the output of the processing is `target`.
// The optional `fee_asset` rides along so the consumption can settle its own fee.
fn create_pass_through_note(
    sender: AccountId,
    target: AccountId,
    asset: Asset,
    fee_asset: Option<Asset>,
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
    let mut note_assets = vec![asset];
    note_assets.extend(fee_asset);
    let note = Note::new(NoteAssets::new(note_assets)?, metadata, pass_through_recipient);

    let pass_through_note_details =
        NoteDetails::new(NoteAssets::new(vec![asset])?, target_recipient);
    Ok((note, pass_through_note_details))
}

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use assert_matches::assert_matches;
use miden_client::account::component::{AccountComponent, AccountComponentMetadata, Approver};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountId,
    AccountType,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotName,
};
use miden_client::assembly::CodeBuilder;
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig, RPO_FALCON_SCHEME_ID};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::standards::NoteSyncHint;
use miden_client::note::{BlockNumber, NoteFile, NoteTag, NoteType};
use miden_client::rpc::domain::account::{
    AccountStorageRequirements,
    GetAccountRequest,
    StorageMapEntries,
    StorageMapFetch,
    VaultFetch,
};
use miden_client::rpc::{GrpcClient, NodeRpcClient};
use miden_client::store::{
    InputNoteRecord,
    InputNoteState,
    NoteFilter,
    OutputNoteState,
    TransactionFilter,
};
use miden_client::testing::account_id::AccountIdBuilder;
use miden_client::testing::common::*;
use miden_client::transaction::{
    DiscardCause,
    PaymentNoteDescription,
    ProvenTransaction,
    TransactionInputs,
    TransactionProver,
    TransactionProverError,
    TransactionRequestBuilder,
    TransactionStatus,
};
use miden_client::{ClientError, Felt, Word};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use tracing::info;

use crate::tests::config::ClientConfig;

pub async fn test_client_builder_initializes_client_with_endpoint(
    client_config: ClientConfig,
) -> Result<()> {
    let (endpoint, _, store_config, auth_path) = client_config.as_parts();

    let mut client = ClientBuilder::<FilesystemKeyStore>::new()
        .grpc_client(&endpoint, Some(10_000))
        .filesystem_keystore(auth_path)?
        .sqlite_store(store_config)
        .in_debug_mode(miden_client::DebugMode::Enabled)
        .build()
        .await?;

    assert!(client.in_debug_mode());

    let sync_summary = client.sync_state().await?;

    assert!(sync_summary.block_num.as_u32() > 0);
    Ok(())
}

pub async fn test_multiple_tx_on_same_block(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await?;
    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First Mint necessary token
    let tx_id =
        mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id).await?;

    // Build two P2ID transfer requests of TRANSFER_AMOUNT each.
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    let tx_request_1 = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    let tx_request_2 = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let note_id_1 = tx_request_1
        .expected_output_own_notes()
        .pop()
        .expect("tx_request_1 should produce one P2ID note")
        .id();
    let note_id_2 = tx_request_2
        .expected_output_own_notes()
        .pop()
        .expect("tx_request_2 should produce one P2ID note")
        .id();

    info!(from = %from_account_id, to = %to_account_id, "Submitting 2-tx P2ID batch");

    // Submit both requests as a single proven batch via the node's `SubmitProvenBatch` path.
    let block_num = client
        .new_transaction_batch()
        .push(from_account_id, tx_request_1)
        .await?
        .push(from_account_id, tx_request_2)
        .await?
        .submit()
        .await?;

    info!(
        submitted_at = block_num.as_u32(),
        "batch submitted; submission_height returned (actual inclusion block may be later)"
    );

    let sender_committed = {
        let mut found: Vec<_> = Vec::new();
        for _ in 0..30 {
            wait_for_blocks(&mut client, 1).await;
            client.sync_state().await?;
            found = client
                .get_transactions(TransactionFilter::All)
                .await?
                .into_iter()
                .filter(|tx| tx.details.account_id == from_account_id)
                .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
                .collect();
            if found.len() >= 3 {
                break;
            }
        }
        found
    };
    assert!(
        sender_committed.len() >= 3,
        "expected at least 3 committed sender transactions (1 mint + 2 batch), got {}",
        sender_committed.len()
    );

    // Same-block guarantee: the 2 batch txs must share a committed block_number.
    let mut per_block: std::collections::BTreeMap<_, usize> = std::collections::BTreeMap::new();
    for tx in &sender_committed {
        if let TransactionStatus::Committed { block_number, .. } = tx.status {
            *per_block.entry(block_number).or_insert(0) += 1;
        }
    }
    assert!(
        per_block.values().any(|&count| count >= 2),
        "expected 2 batch txs to share a committed block_number, got per-block counts: {per_block:?}"
    );

    // Both P2ID output notes must be persisted as CommittedFull after apply.
    let note_1 = client.get_output_note(note_id_1).await?.expect("note 1 missing");
    let note_2 = client.get_output_note(note_id_2).await?.expect("note 2 missing");
    assert!(matches!(note_1.state(), OutputNoteState::CommittedFull { .. }));
    assert!(matches!(note_2.state(), OutputNoteState::CommittedFull { .. }));

    let sender_balance = client
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .context("failed to find sender account after transactions")?;
    assert_eq!(sender_balance, MINT_AMOUNT - (TRANSFER_AMOUNT * 2));
    Ok(())
}

pub async fn test_import_expected_notes(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, authenticator_1) = client_config.clone().into_client().await?;
    let (first_basic_account, faucet_account) = setup_wallet_and_faucet(
        &mut client_1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (mut client_2, authenticator_2) = client_config.into_client().await?;
    let (client_2_account, _) = insert_new_wallet(
        &mut client_2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    wait_for_node(&mut client_2).await;

    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet_account.id(), MINT_AMOUNT).unwrap(),
            client_2_account.id(),
            NoteType::Public,
            client_2.rng(),
        )
        .unwrap();
    let note: InputNoteRecord =
        tx_request.expected_output_own_notes().pop().unwrap().clone().into();
    client_2.sync_state().await.unwrap();

    // Importing a public note before it's committed onchain should fail
    assert_eq!(
        client_2
            .import_notes(&[NoteFile::NoteId(note.id().unwrap())])
            .await
            .unwrap_err()
            .to_string(),
        "note import error: No notes fetched from node".to_string()
    );
    execute_tx_and_sync(&mut client_1, faucet_account.id(), tx_request).await?;

    // Use client 1 to wait until a couple of blocks have passed
    wait_for_blocks(&mut client_1, 3).await;

    let new_sync_data = client_2.sync_state().await.unwrap();

    client_2.add_note_tag(note.metadata().unwrap().tag()).await.unwrap();
    client_2
        .import_notes(&[NoteFile::NoteId(note.clone().id().unwrap())])
        .await
        .unwrap();
    client_2.sync_state().await.unwrap();
    let input_note = client_2.get_input_note(note.id().unwrap()).await?.unwrap();
    // If imported after execution and syncing then the inclusion proof should be Some
    assert!(input_note.inclusion_proof().is_some(), "Expected inclusion proof to be present");

    assert!(
        new_sync_data.block_num > input_note.inclusion_proof().unwrap().location().block_num() + 1
    );

    // If client 2 successfully consumes the note, we confirm we have MMR and block header data
    let tx_id =
        consume_notes(&mut client_2, client_2_account.id(), &[input_note.try_into().unwrap()])
            .await;
    wait_for_tx(&mut client_2, tx_id).await?;

    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet_account.id(), MINT_AMOUNT).unwrap(),
            first_basic_account.id(),
            NoteType::Private,
            client_2.rng(),
        )
        .unwrap();
    let note: InputNoteRecord =
        tx_request.expected_output_own_notes().pop().unwrap().clone().into();

    // Import the node before it's committed onchain works if we have full `NoteDetails`
    client_2.add_note_tag(note.metadata().unwrap().tag()).await.unwrap();
    client_2
        .import_notes(&[NoteFile::ExpectedNote {
            details: note.clone().into(),
            sync_hint: NoteSyncHint::new(
                client_1.get_sync_height().await.unwrap(),
                note.metadata().unwrap().tag(),
            ),
        }])
        .await
        .unwrap();
    // Look up by details commitment: an `Expected` note has no metadata yet, so its `note_id`
    // is unset and it cannot be resolved via `get_input_note`.
    let input_note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![note.details_commitment()]))
        .await?
        .pop()
        .unwrap();

    // If imported before execution, the note should be imported in `Expected` state
    assert!(matches!(input_note.state(), InputNoteState::Expected { .. }));

    execute_tx_and_sync(&mut client_1, faucet_account.id(), tx_request).await?;
    client_2.sync_state().await.unwrap();

    // After sync, the imported note should have inclusion proof even if it's not relevant for its
    // accounts.
    let input_note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![note.details_commitment()]))
        .await?
        .pop()
        .unwrap();
    assert!(input_note.inclusion_proof().is_some(), "Expected inclusion proof to be present");

    // If inclusion proof is invalid this should panic
    let tx_id =
        consume_notes(&mut client_1, first_basic_account.id(), &[input_note.try_into().unwrap()])
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;
    Ok(())
}

pub async fn test_import_expected_note_uncommitted(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, authenticator) = client_config.clone().into_client().await?;
    let faucet_account = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap()
    .0;

    let (mut client_2, _) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    let (client_2_account, _) = insert_new_wallet(
        &mut client_2,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    wait_for_node(&mut client_2).await;

    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        FungibleAsset::new(faucet_account.id(), MINT_AMOUNT).unwrap(),
        client_2_account.id(),
        NoteType::Public,
        client_1.rng(),
    )?;

    let note: InputNoteRecord =
        tx_request.expected_output_own_notes().pop().unwrap().clone().into();
    client_2.sync_state().await.unwrap();

    // If the verification is requested before execution then the import should fail
    let imported_commitment = client_2
        .import_notes(&[NoteFile::ExpectedNote {
            details: note.clone().into(),
            sync_hint: NoteSyncHint::new(0.into(), note.metadata().unwrap().tag()),
        }])
        .await?[0];

    let imported_note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![imported_commitment]))
        .await?
        .pop()
        .unwrap();

    assert!(matches!(imported_note.state(), InputNoteState::Expected { .. }));
    Ok(())
}

pub async fn test_import_expected_notes_from_the_past_as_committed(
    client_config: ClientConfig,
) -> Result<()> {
    let (mut client_1, authenticator_1) = client_config.clone().into_client().await?;
    let (first_basic_account, faucet_account) = setup_wallet_and_faucet(
        &mut client_1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (mut client_2, _) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    wait_for_node(&mut client_2).await;

    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        FungibleAsset::new(faucet_account.id(), MINT_AMOUNT).unwrap(),
        first_basic_account.id(),
        NoteType::Public,
        client_1.rng(),
    )?;
    let note: InputNoteRecord =
        tx_request.expected_output_own_notes().pop().unwrap().clone().into();

    let block_height_before = client_1.get_sync_height().await.unwrap();

    execute_tx_and_sync(&mut client_1, faucet_account.id(), tx_request).await?;

    // importing the note before client_2 is synced will result in a note with `Expected` state
    let commitment = client_2
        .import_notes(&[NoteFile::ExpectedNote {
            details: note.clone().into(),
            sync_hint: NoteSyncHint::new(block_height_before, note.metadata().unwrap().tag()),
        }])
        .await?[0];

    let imported_note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![commitment]))
        .await?
        .pop()
        .unwrap();

    assert!(matches!(imported_note.state(), InputNoteState::Expected { .. }));

    client_2.sync_state().await.unwrap();

    // Note already imported
    assert!(
        client_2
            .import_notes(&[NoteFile::ExpectedNote {
                details: note.clone().into(),
                sync_hint: NoteSyncHint::new(block_height_before, note.metadata().unwrap().tag()),
            }])
            .await?
            .is_empty()
    );

    let imported_note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![commitment]))
        .await?
        .pop()
        .unwrap();

    // Get the note status in client 1
    let client_1_note = client_1
        .get_input_notes(NoteFilter::DetailsCommitments(vec![commitment]))
        .await?
        .pop()
        .unwrap();

    assert_eq!(imported_note.state(), client_1_note.state());
    assert!(matches!(imported_note.state(), InputNoteState::Committed { .. }));
    Ok(())
}

pub async fn test_get_account_update(client_config: ClientConfig) -> Result<()> {
    // Create a client with both public and private accounts.
    let (mut client, authenticator) = client_config.clone().into_client().await?;

    let (basic_wallet_1, faucet_account) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    wait_for_node(&mut client).await;

    let (basic_wallet_2, ..) =
        insert_new_wallet(&mut client, AccountType::Public, &authenticator, RPO_FALCON_SCHEME_ID)
            .await?;

    // Mint and consume notes with both accounts so they are included in the node.
    let tx_id_1 =
        mint_and_consume(&mut client, basic_wallet_1.id(), faucet_account.id(), NoteType::Private)
            .await;
    wait_for_tx(&mut client, tx_id_1).await?;
    let tx_id_2 =
        mint_and_consume(&mut client, basic_wallet_2.id(), faucet_account.id(), NoteType::Private)
            .await;
    wait_for_tx(&mut client, tx_id_2).await?;

    // Request updates from node for both accounts. The request should not fail and both types of
    // [`AccountDetails`] should be received.
    // TODO: should we expose the `get_account_update` endpoint from the Client?
    let rpc_api = client.test_rpc_api();
    let details1 = rpc_api.get_account_details(basic_wallet_1.id()).await.unwrap();
    let details2 = rpc_api.get_account_details(basic_wallet_2.id()).await.unwrap();

    assert!(details1.is_none());
    assert_matches!(details2, Some(account) if {
        account.vault().assets().any(|asset| matches!(
            asset,
            miden_client::asset::Asset::Fungible(fa)
                if fa.faucet_id() == faucet_account.id() && fa.amount().as_u64() == MINT_AMOUNT
        ))
    });
    Ok(())
}

pub async fn test_sync_detail_values(client_config: ClientConfig) -> Result<()> {
    let (mut client1, authenticator_1) = client_config.clone().into_client().await?;
    let (mut client2, authenticator_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    wait_for_node(&mut client1).await;
    wait_for_node(&mut client2).await;

    let (first_regular_account, faucet_account_header) = setup_wallet_and_faucet(
        &mut client1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (second_regular_account, ..) = insert_new_wallet(
        &mut client2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First Mint necessary token
    let tx_id =
        mint_and_consume(&mut client1, from_account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client1, tx_id).await?;

    // Second client sync shouldn't have any new changes
    let new_details = client2.sync_state().await.unwrap();
    assert!(new_details.is_empty());

    // Do a transfer with recall from first account to second account
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(vec![Asset::Fungible(asset)], from_account_id, to_account_id)
            .with_reclaim_height(new_details.block_num + 5),
        NoteType::Public,
        client1.rng(),
    )?;
    let note = tx_request.expected_output_own_notes().pop().unwrap();
    execute_tx_and_sync(&mut client1, from_account_id, tx_request).await?;

    // Second client sync should have new note
    let new_details = client2.sync_state().await.unwrap();
    assert_eq!(new_details.new_public_notes.len(), 1);
    assert_eq!(new_details.committed_notes.len(), 0);
    assert_eq!(new_details.consumed_notes.len(), 0);
    assert_eq!(new_details.updated_accounts.len(), 0);

    // Consume the note with the second account
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note]).unwrap();
    execute_tx_and_sync(&mut client2, to_account_id, tx_request).await?;

    // First client sync should have a new nullifier as the note was consumed
    let new_details = client1.sync_state().await.unwrap();
    assert_eq!(new_details.committed_notes.len(), 0);
    assert_eq!(new_details.consumed_notes.len(), 1);
    Ok(())
}

/// Verifies the client chunks for an over-the-limit `sync_notes` request
pub async fn test_sync_notes_chunks_when_exceeding_limits(
    client_config: ClientConfig,
) -> Result<()> {
    let rpc_endpoint = client_config.rpc_endpoint.clone();
    let rpc_timeout = client_config.rpc_timeout_ms;
    let (mut client, authenticator) = client_config.into_client().await?;

    let (wallet, faucet) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let fungible_asset = FungibleAsset::new(faucet.id(), MINT_AMOUNT)?;
    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        wallet.id(),
        NoteType::Public,
        client.rng(),
    )?;
    let minted_note = tx_request.expected_output_own_notes().pop().unwrap();
    execute_tx_and_sync(&mut client, faucet.id(), tx_request).await?;

    let real_tag = minted_note.metadata().tag();

    let grpc = GrpcClient::new(&rpc_endpoint, rpc_timeout);
    let limits = grpc.get_rpc_limits().await?;
    let sync_height = client.get_sync_height().await?;

    let mut tags: BTreeSet<NoteTag> = (0..limits.note_tags_limit).map(NoteTag::new).collect();
    tags.insert(real_tag);
    let blocks = grpc.sync_notes(BlockNumber::from(0u32), sync_height, &tags).await?;
    assert!(tags.len() as u32 > limits.note_tags_limit);
    assert!(
        blocks.iter().any(|b| b.notes.contains_key(&minted_note.id())),
        "expected the minted note in the sync_notes response",
    );

    Ok(())
}

/// Verifies the client chunks for an over-the-limit `sync_transactions` request
pub async fn test_sync_transactions_chunks_when_exceeding_limits(
    client_config: ClientConfig,
) -> Result<()> {
    let rpc_endpoint = client_config.rpc_endpoint.clone();
    let rpc_timeout = client_config.rpc_timeout_ms;
    let (mut client, authenticator) = client_config.into_client().await?;

    let (wallet, faucet) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let fungible_asset = FungibleAsset::new(faucet.id(), MINT_AMOUNT)?;
    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        wallet.id(),
        NoteType::Public,
        client.rng(),
    )?;
    let tx_id = client.submit_new_transaction(faucet.id(), tx_request).await?;
    wait_for_tx(&mut client, tx_id).await?;

    let grpc = GrpcClient::new(&rpc_endpoint, rpc_timeout);
    let limits = grpc.get_rpc_limits().await?;
    let sync_height = client.get_sync_height().await?;

    let rng = client.rng();
    let mut account_ids: Vec<AccountId> = (0..limits.account_ids_limit)
        .map(|_| AccountIdBuilder::new().build_with_rng(rng))
        .collect();
    account_ids.push(faucet.id());
    let txs = grpc
        .sync_transactions(BlockNumber::from(0u32), sync_height, account_ids.clone())
        .await?;
    assert!(account_ids.len() as u32 > limits.account_ids_limit);
    assert!(
        txs.iter().any(|t| t.transaction_header.id() == tx_id),
        "expected the executed transaction in the sync_transactions response",
    );

    Ok(())
}

/// This test runs 3 mint transactions that get included in different blocks so that once we sync
/// we can check that each transaction gets marked as committed in the corresponding block.
pub async fn test_multiple_transactions_can_be_committed_in_different_blocks_without_sync(
    client_config: ClientConfig,
) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;

    let (first_regular_account, faucet_account_header) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let from_account_id = first_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Mint first note
    let (first_note_id, first_note_tx_id) = {
        // Create a Mint Tx for 1000 units of our fungible asset
        let fungible_asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap();

        info!(faucet_id = %faucet_account_id, target = %from_account_id, "Minting first asset");
        let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            from_account_id,
            NoteType::Private,
            client.rng(),
        )?;

        info!("Executing first mint transaction");
        let transaction_id =
            client.submit_new_transaction(faucet_account_id, tx_request.clone()).await?;
        let note_id = tx_request.expected_output_own_notes().pop().unwrap().id();

        (note_id, transaction_id)
    };

    // Mint second note
    let (second_note_id, second_note_tx_id) = {
        // Create a Mint Tx for 1000 units of our fungible asset
        let fungible_asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap();

        info!(faucet_id = %faucet_account_id, target = %from_account_id, "Minting second asset");
        let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            from_account_id,
            NoteType::Private,
            client.rng(),
        )?;

        info!("Executing second mint transaction");
        let transaction_result =
            client.execute_transaction(faucet_account_id, tx_request.clone()).await.unwrap();
        let transaction_id = transaction_result.id();

        info!(tx_id = %transaction_id, "Sending second transaction to node");
        // May need a few attempts until it gets included
        let note_id = tx_request.expected_output_own_notes().pop().unwrap().id();
        while client
            .test_rpc_api()
            .get_notes_by_id(&[first_note_id])
            .await
            .unwrap()
            .is_empty()
        {
            std::thread::sleep(Duration::from_secs(3));
        }
        let proven_transaction = client.prove_transaction(&transaction_result).await.unwrap();
        let submission_height = client
            .submit_proven_transaction(proven_transaction, &transaction_result)
            .await
            .unwrap();
        client.apply_transaction(&transaction_result, submission_height).await.unwrap();

        (note_id, transaction_id)
    };

    // Mint third note
    let (third_note_id, third_note_tx_id) = {
        // Create a Mint Tx for 1000 units of our fungible asset
        let fungible_asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap();

        info!(faucet_id = %faucet_account_id, target = %from_account_id, "Minting third asset");
        let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            from_account_id,
            NoteType::Private,
            client.rng(),
        )?;

        info!("Executing third mint transaction");
        let transaction_result =
            client.execute_transaction(faucet_account_id, tx_request.clone()).await.unwrap();
        let transaction_id = transaction_result.id();

        info!(tx_id = %transaction_id, "Sending third transaction to node");
        // May need a few attempts until it gets included
        let note_id = tx_request.expected_output_own_notes().pop().unwrap().id();
        while client
            .test_rpc_api()
            .get_notes_by_id(&[second_note_id])
            .await
            .unwrap()
            .is_empty()
        {
            std::thread::sleep(Duration::from_secs(3));
        }
        let proven_transaction = client.prove_transaction(&transaction_result).await.unwrap();
        let submission_height = client
            .submit_proven_transaction(proven_transaction, &transaction_result)
            .await
            .unwrap();
        client.apply_transaction(&transaction_result, submission_height).await.unwrap();

        (note_id, transaction_id)
    };

    // Wait until the note gets committed in the node (without syncing)
    while client
        .test_rpc_api()
        .get_notes_by_id(&[third_note_id])
        .await
        .unwrap()
        .is_empty()
    {
        std::thread::sleep(Duration::from_secs(3));
    }

    client.sync_state().await.unwrap();

    let all_transactions = client.get_transactions(TransactionFilter::All).await.unwrap();
    let first_tx = all_transactions.iter().find(|tx| tx.id == first_note_tx_id).unwrap();
    let second_tx = all_transactions.iter().find(|tx| tx.id == second_note_tx_id).unwrap();
    let third_tx = all_transactions.iter().find(|tx| tx.id == third_note_tx_id).unwrap();

    match (first_tx.status.clone(), second_tx.status.clone(), third_tx.status.clone()) {
        (
            TransactionStatus::Committed { block_number: first_tx_commit_height, .. },
            TransactionStatus::Committed {
                block_number: second_tx_commit_height, ..
            },
            TransactionStatus::Committed { block_number: third_tx_commit_height, .. },
        ) => {
            assert!(first_tx_commit_height < second_tx_commit_height);
            assert!(second_tx_commit_height < third_tx_commit_height);
        },
        _ => {
            panic!("All three TXs should be committed in different blocks")
        },
    }
    Ok(())
}

/// Test that checks multiple features:
/// - Consuming multiple notes in a single transaction.
/// - Consuming authenticated notes.
/// - Consuming unauthenticated notes.
pub async fn test_consume_multiple_expected_notes(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator_1) = client_config.clone().into_client().await?;
    let (mut unauth_client, authenticator_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    wait_for_node(&mut client).await;

    // Setup accounts
    let (target_basic_account_1, faucet_account_header) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (target_basic_account_2, ..) = insert_new_wallet(
        &mut unauth_client,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    unauth_client.sync_state().await.unwrap();

    let faucet_account_id = faucet_account_header.id();
    let to_account_ids = [target_basic_account_1.id(), target_basic_account_2.id()];

    let fungible_asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    // Mint tokens to the accounts
    let mint_tx_request = mint_multiple_fungible_asset(
        fungible_asset,
        &[to_account_ids[0], to_account_ids[0], to_account_ids[1], to_account_ids[1]],
        NoteType::Private,
        client.rng(),
    );
    let all_expected_notes = mint_tx_request.expected_output_own_notes();
    execute_tx_and_sync(&mut client, faucet_account_id, mint_tx_request).await?;

    unauth_client.sync_state().await.unwrap();

    // Filter notes by ownership
    let expected_notes = all_expected_notes.into_iter();
    let client_notes: Vec<_> = client.get_input_notes(NoteFilter::All).await.unwrap();
    let client_notes_ids: Vec<_> = client_notes.iter().filter_map(|note| note.id()).collect();

    let (client_owned_notes, unauth_owned_notes): (Vec<_>, Vec<_>) =
        expected_notes.partition(|note| client_notes_ids.contains(&note.id()));

    // Create and execute transactions
    let tx_request_1 = TransactionRequestBuilder::new()
        .input_notes(client_owned_notes.iter().map(|note| (note.clone(), None)))
        .build()?;

    let tx_request_2 = TransactionRequestBuilder::new()
        .input_notes(unauth_owned_notes.iter().map(|note| ((*note).clone(), None)))
        .build()?;

    let tx_id_1 = client.submit_new_transaction(to_account_ids[0], tx_request_1).await.unwrap();
    let tx_id_2 = unauth_client
        .submit_new_transaction(to_account_ids[1], tx_request_2)
        .await
        .unwrap();

    // Ensure notes are processed
    assert!(!client.get_input_notes(NoteFilter::Processing).await.unwrap().is_empty());
    assert!(!unauth_client.get_input_notes(NoteFilter::Processing).await.unwrap().is_empty());

    wait_for_tx(&mut client, tx_id_1).await?;
    wait_for_tx(&mut unauth_client, tx_id_2).await?;

    // Verify no remaining expected notes and all notes are consumed
    assert!(client.get_input_notes(NoteFilter::Expected).await.unwrap().is_empty());
    assert!(unauth_client.get_input_notes(NoteFilter::Expected).await.unwrap().is_empty());

    assert!(
        !client.get_input_notes(NoteFilter::Consumed).await.unwrap().is_empty(),
        "Authenticated notes are consumed"
    );
    assert!(
        !unauth_client.get_input_notes(NoteFilter::Consumed).await.unwrap().is_empty(),
        "Unauthenticated notes are consumed"
    );

    // Validate the final asset amounts in each account
    for (client, account_id) in [(client, to_account_ids[0]), (unauth_client, to_account_ids[1])] {
        assert_account_has_single_asset(
            &client,
            account_id,
            faucet_account_id,
            TRANSFER_AMOUNT * 2,
        )
        .await;
    }
    Ok(())
}

pub async fn test_import_consumed_note_with_proof(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, authenticator_1) = client_config.clone().into_client().await?;
    let (first_regular_account, faucet_account_header) = setup_wallet_and_faucet(
        &mut client_1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (mut client_2, authenticator_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    let (client_2_account, _) = insert_new_wallet(
        &mut client_2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    wait_for_node(&mut client_2).await;

    let from_account_id = first_regular_account.id();
    let to_account_id = client_2_account.id();
    let faucet_account_id = faucet_account_header.id();

    let tx_id =
        mint_and_consume(&mut client_1, from_account_id, faucet_account_id, NoteType::Private)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    let current_block_num = client_1.get_sync_height().await.unwrap();
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    info!(from = %from_account_id, to = %to_account_id, "Running P2IDE transaction");
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(vec![Asset::Fungible(asset)], from_account_id, to_account_id)
            .with_reclaim_height(current_block_num),
        NoteType::Private,
        client_1.rng(),
    )?;
    execute_tx_and_sync(&mut client_1, from_account_id, tx_request).await?;
    let note = client_1
        .get_input_notes(NoteFilter::Committed)
        .await
        .unwrap()
        .first()
        .unwrap()
        .clone();

    // Consume the note with the sender account

    info!(note_id = %note.id().unwrap(), account_id = %from_account_id, "Consuming note");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone().try_into().unwrap()])
        .unwrap();
    execute_tx_and_sync(&mut client_1, from_account_id, tx_request).await?;

    // Import the consumed note
    client_2
        .import_notes(&[NoteFile::Committed {
            note: note.clone().try_into().unwrap(),
            proof: note.inclusion_proof().unwrap().clone(),
        }])
        .await?;

    // Look up the consumed note by its details commitment, which is stable across state
    // transitions.
    let consumed_note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![note.details_commitment()]))
        .await?
        .pop()
        .unwrap();
    assert!(matches!(consumed_note.state(), InputNoteState::ConsumedExternal { .. }));
    Ok(())
}

pub async fn test_import_consumed_note_with_id(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, authenticator) = client_config.clone().into_client().await?;
    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client_1,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await?;

    let (mut client_2, _) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    wait_for_node(&mut client_2).await;

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    let tx_id =
        mint_and_consume(&mut client_1, from_account_id, faucet_account_id, NoteType::Private)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    let current_block_num = client_1.get_sync_height().await.unwrap();
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    info!(from = %from_account_id, to = %to_account_id, "Running P2IDE transaction (public)");
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(vec![Asset::Fungible(asset)], from_account_id, to_account_id)
            .with_reclaim_height(current_block_num),
        NoteType::Public,
        client_1.rng(),
    )?;
    execute_tx_and_sync(&mut client_1, from_account_id, tx_request).await?;
    let note = client_1
        .get_input_notes(NoteFilter::Committed)
        .await
        .unwrap()
        .first()
        .unwrap()
        .clone();

    // Consume the note with the sender account

    info!(note_id = %note.id().unwrap(), account_id = %from_account_id, "Consuming note");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone().try_into().unwrap()])
        .unwrap();
    execute_tx_and_sync(&mut client_1, from_account_id, tx_request).await?;
    client_2.sync_state().await.unwrap();

    // Import the consumed note
    client_2.import_notes(&[NoteFile::NoteId(note.id().unwrap())]).await.unwrap();

    // Look up the consumed note by its details commitment, which is stable across state
    // transitions.
    let consumed_note = client_2
        .get_input_notes(NoteFilter::DetailsCommitments(vec![note.details_commitment()]))
        .await?
        .pop()
        .unwrap();
    assert!(matches!(consumed_note.state(), InputNoteState::ConsumedExternal { .. }));
    Ok(())
}

pub async fn test_import_note_with_proof(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, authenticator) = client_config.clone().into_client().await?;
    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client_1,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await?;

    let (mut client_2, _) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;

    wait_for_node(&mut client_2).await;

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    let tx_id =
        mint_and_consume(&mut client_1, from_account_id, faucet_account_id, NoteType::Private)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    let current_block_num = client_1.get_sync_height().await.unwrap();
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    info!(from = %from_account_id, to = %to_account_id, "Running P2IDE transaction (with proof)");
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(vec![Asset::Fungible(asset)], from_account_id, to_account_id)
            .with_reclaim_height(current_block_num),
        NoteType::Private,
        client_1.rng(),
    )?;
    execute_tx_and_sync(&mut client_1, from_account_id, tx_request).await?;

    let note = client_1
        .get_input_notes(NoteFilter::Committed)
        .await
        .unwrap()
        .first()
        .unwrap()
        .clone();

    // Import the consumed note
    client_2
        .import_notes(&[NoteFile::Committed {
            note: note.clone().try_into().unwrap(),
            proof: note.inclusion_proof().unwrap().clone(),
        }])
        .await?;

    let imported_note = client_2.get_input_note(note.id().unwrap()).await?.unwrap();
    assert!(matches!(imported_note.state(), InputNoteState::Unverified { .. }));

    client_2.sync_state().await.unwrap();
    let imported_note = client_2.get_input_note(note.id().unwrap()).await?.unwrap();
    assert!(matches!(imported_note.state(), InputNoteState::Committed { .. }));
    Ok(())
}

pub async fn test_discarded_transaction(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, authenticator_1) = client_config.clone().into_client().await?;
    let (first_regular_account, faucet_account_header) = setup_wallet_and_faucet(
        &mut client_1,
        AccountType::Private,
        &authenticator_1,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (mut client_2, authenticator_2) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    let (second_regular_account, ..) = insert_new_wallet(
        &mut client_2,
        AccountType::Private,
        &authenticator_2,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    wait_for_node(&mut client_2).await;

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    let tx_id =
        mint_and_consume(&mut client_1, from_account_id, faucet_account_id, NoteType::Private)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    let current_block_num = client_1.get_sync_height().await.unwrap();
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    info!(from = %from_account_id, to = %to_account_id, "Running P2IDE transaction (discarded)");
    let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
        PaymentNoteDescription::new(vec![Asset::Fungible(asset)], from_account_id, to_account_id)
            .with_reclaim_height(current_block_num),
        NoteType::Public,
        client_1.rng(),
    )?;

    execute_tx_and_sync(&mut client_1, from_account_id, tx_request).await?;
    client_2.sync_state().await.unwrap();
    let note = client_1
        .get_input_notes(NoteFilter::Committed)
        .await
        .unwrap()
        .first()
        .unwrap()
        .clone();

    info!(note_id = %note.id().unwrap(), account_id = %from_account_id, "Consuming note (without submitting)");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone().try_into().unwrap()])
        .unwrap();

    // Consume the note in client 1 but dont submit it to the node
    let transaction_result =
        client_1.execute_transaction(from_account_id, tx_request.clone()).await.unwrap();
    let tx_id = transaction_result.id();

    // Store the account state before applying the transaction
    let account_hash_before_tx =
        client_1.account_reader(from_account_id).commitment().await.unwrap();

    // Apply the transaction
    let submission_height = client_1.get_sync_height().await.unwrap();
    client_1
        .apply_transaction(&transaction_result, submission_height)
        .await
        .unwrap();

    // Check that the account state has changed after applying the transaction
    let account_hash_after_tx =
        client_1.account_reader(from_account_id).commitment().await.unwrap();

    assert_ne!(
        account_hash_before_tx, account_hash_after_tx,
        "Account hash should change after applying the transaction"
    );

    let note_record = client_1.get_input_note(note.id().unwrap()).await?.unwrap();
    assert!(matches!(note_record.state(), InputNoteState::ProcessingAuthenticated(_)));

    // Consume the note in client 2
    execute_tx_and_sync(&mut client_2, to_account_id, tx_request).await?;

    let note_record = client_2.get_input_note(note.id().unwrap()).await?.unwrap();
    assert!(matches!(note_record.state(), InputNoteState::ConsumedAuthenticatedLocal(_)));

    // After sync the note in client 1 should be consumed externally and the transaction discarded.
    // Look the note up by its details commitment, which is stable across state transitions.
    client_1.sync_state().await.unwrap();
    let note_record = client_1
        .get_input_notes(NoteFilter::DetailsCommitments(vec![note.details_commitment()]))
        .await?
        .pop()
        .unwrap();
    assert!(matches!(note_record.state(), InputNoteState::ConsumedExternal(_)));
    let tx_record = client_1
        .get_transactions(TransactionFilter::All)
        .await
        .unwrap()
        .into_iter()
        .find(|tx| tx.id == tx_id)
        .with_context(|| {
            format!("Transaction with id {tx_id} not found in discarded transactions")
        })?;
    assert!(matches!(
        tx_record.status,
        TransactionStatus::Discarded(DiscardCause::InputConsumed)
    ));

    // Check that the account state has been rolled back after the transaction was discarded
    let account_hash_after_sync =
        client_1.account_reader(from_account_id).commitment().await.unwrap();

    assert_ne!(
        account_hash_after_sync, account_hash_after_tx,
        "Account hash should change after transaction was discarded"
    );
    assert_eq!(
        account_hash_after_sync, account_hash_before_tx,
        "Account hash should be rolled back to the value before the transaction"
    );
    Ok(())
}

struct AlwaysFailingProver;

impl AlwaysFailingProver {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl TransactionProver for AlwaysFailingProver {
    async fn prove(
        &self,
        _inputs: TransactionInputs,
    ) -> Result<ProvenTransaction, TransactionProverError> {
        return Err(TransactionProverError::other("This prover always fails"));
    }
}

pub async fn test_custom_transaction_prover_error_caught(
    client_config: ClientConfig,
) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    let (first_regular_account, faucet_account_header) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let from_account_id = first_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    let fungible_asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap();

    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        from_account_id,
        NoteType::Private,
        client.rng(),
    )?;

    let transaction_result =
        client.execute_transaction(faucet_account_id, tx_request.clone()).await.unwrap();

    let result = client
        .prove_transaction_with(&transaction_result, Arc::new(AlwaysFailingProver::new()))
        .await;

    let Err(ClientError::TransactionProvingError(TransactionProverError::Other {
        error_msg, ..
    })) = result
    else {
        panic!("expected different prover error");
    };
    assert_eq!(error_msg.as_ref(), "This prover always fails");
    Ok(())
}

pub async fn test_locked_account(client_config: ClientConfig) -> Result<()> {
    let (mut client_1, authenticator) = client_config.clone().into_client().await?;

    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client_1,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (private_account, _) = insert_new_wallet(
        &mut client_1,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let from_account_id = private_account.id();
    let faucet_account_id = faucet_account.id();

    wait_for_node(&mut client_1).await;

    let tx_id =
        mint_and_consume(&mut client_1, from_account_id, faucet_account_id, NoteType::Private)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // Get full account from store for export to client_2
    let private_account: Account =
        client_1.get_account(from_account_id).await?.context("Account not found")?;

    let original_seed = private_account.seed();

    // Import private account in client 2
    let (mut client_2, _) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    client_2.add_account(&private_account, false).await.unwrap();

    wait_for_node(&mut client_2).await;

    // When imported the account shouldn't be locked
    assert!(!client_2.account_reader(from_account_id).status().await.unwrap().is_locked());

    // Consume note with private account in client 1
    let tx_id =
        mint_and_consume(&mut client_1, from_account_id, faucet_account_id, NoteType::Private)
            .await;
    wait_for_tx(&mut client_1, tx_id).await?;

    // After sync the private account should be locked in client 2
    let summary = client_2.sync_state().await.unwrap();
    assert!(summary.locked_accounts.contains(&from_account_id));
    let status = client_2.account_reader(from_account_id).status().await.unwrap();
    assert!(status.is_locked());
    assert_eq!(status.seed(), original_seed.as_ref());

    // Get updated account from client 1 and import it in client 2 with `overwrite` flag
    let updated_private_account: Account =
        client_1.get_account(from_account_id).await?.context("Account not found")?;
    client_2.add_account(&updated_private_account, true).await.unwrap();

    // After sync the private account shouldn't be locked in client 2
    client_2.sync_state().await.unwrap();
    assert!(!client_2.account_reader(from_account_id).status().await.unwrap().is_locked());
    Ok(())
}

pub async fn test_expired_transaction_fails(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    let (faucet_account, _) = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let (private_account, ..) =
        insert_new_wallet(&mut client, AccountType::Private, &authenticator, RPO_FALCON_SCHEME_ID)
            .await?;

    let from_account_id = private_account.id();
    let faucet_account_id = faucet_account.id();

    wait_for_node(&mut client).await;

    let expiration_delta = 2;

    // Create a Mint Tx for 1000 units of our fungible asset
    let fungible_asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap();
    info!(faucet_id = %faucet_account_id, target = %from_account_id, expiration_delta, "Minting asset with expiration");
    let tx_request = TransactionRequestBuilder::new()
        .expiration_delta(expiration_delta)
        .build_mint_fungible_asset(
            fungible_asset,
            from_account_id,
            NoteType::Public,
            client.rng(),
        )?;

    info!("Executing transaction");
    let transaction_result =
        client.execute_transaction(faucet_account_id, tx_request).await.unwrap();

    info!(tx_id = %transaction_result.id(), "Transaction executed, waiting for expiration");
    wait_for_blocks(&mut client, (expiration_delta + 1).into()).await;

    info!("Sending expired transaction to node (expecting failure)");
    let proven_transaction = client.prove_transaction(&transaction_result).await.unwrap();
    let submitted_tx_result =
        match client.submit_proven_transaction(proven_transaction, &transaction_result).await {
            Ok(submission_height) => {
                client.apply_transaction(&transaction_result, submission_height).await
            },
            Err(err) => Err(err),
        };

    assert!(submitted_tx_result.is_err());
    Ok(())
}

/// Tests that RPC methods that are not directly related to the client logic
/// (like GetBlockByNumber) work correctly
pub async fn test_unused_rpc_api(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.into_client().await?;

    let (first_basic_account, faucet_account) =
        setup_wallet_and_faucet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;

    wait_for_node(&mut client).await;
    client.sync_state().await.unwrap();

    let first_block_num = client.get_sync_height().await.unwrap();

    let (block_header, _) = client
        .test_rpc_api()
        .get_block_header_by_number(Some(first_block_num), false)
        .await?;
    let block = client.test_rpc_api().get_block_by_number(first_block_num, false).await.unwrap();

    assert_eq!(&block_header, block.header());

    let (tx_id, note) =
        mint_note(&mut client, first_basic_account.id(), faucet_account.id(), NoteType::Public)
            .await;
    wait_for_tx(&mut client, tx_id).await?;

    let tx_id =
        consume_notes(&mut client, first_basic_account.id(), std::slice::from_ref(&note)).await;
    wait_for_tx(&mut client, tx_id).await?;

    // Test get_account retrieval (account must be deployed on-chain first)
    let (proof_block_num, account_proof) = client
        .test_rpc_api()
        .get_account(first_basic_account.id(), GetAccountRequest::new())
        .await?;
    assert!(proof_block_num >= first_block_num);
    assert_eq!(account_proof.account_id(), first_basic_account.id());
    assert!(account_proof.account_header().is_some());

    // The witness's merkle path should resolve to the account root committed
    // in the block header for `proof_block_num`.
    let (proof_block_header, _) = client
        .test_rpc_api()
        .get_block_header_by_number(Some(proof_block_num), false)
        .await?;
    let computed_account_root = account_proof.account_witness().clone().into_proof().compute_root();
    assert_eq!(computed_account_root, proof_block_header.account_root());

    // Define the account code for the custom library
    let custom_code = r#"
        use miden::protocol::native_account
        use miden::core::word

        const MAP_SLOT = word("miden::testing::client::map")

        pub proc update_map
            push.1.2.3.4
            # => [VALUE]
            push.0.0.0.0
            # => [KEY, VALUE]
            push.MAP_SLOT[0..2]
            exec.native_account::set_map_item
            dropw dropw dropw dropw
        end
    "#;

    let mut storage_map = StorageMap::new();
    storage_map.insert(
        StorageMapKey::new(
            [Felt::from(1u32), Felt::from(2u32), Felt::from(3u32), Felt::from(4u32)].into(),
        ),
        [Felt::from(1u32), Felt::from(0u32), Felt::from(0u32), Felt::from(0u32)].into(),
    )?;

    let map_slot_name =
        StorageSlotName::new("miden::testing::client::map").expect("slot name should be valid");
    let storage_slots = vec![StorageSlot::with_map(map_slot_name, storage_map)];
    let (account_with_map_item, _) = insert_account_with_custom_component(
        &mut client,
        custom_code,
        storage_slots,
        AccountType::Public,
        &keystore,
    )
    .await?;

    client.sync_state().await.unwrap();

    let tx_script = CodeBuilder::new()
        .with_linked_module("custom_library::set_map_item_library", custom_code)?
        .compile_tx_script(
            "
        use custom_library::set_map_item_library

        begin
             call.set_map_item_library::update_map
        end
        ",
        )?;

    let tx_request = TransactionRequestBuilder::new().custom_script(tx_script).build()?;
    execute_tx_and_sync(&mut client, account_with_map_item.id(), tx_request.clone()).await?;

    // Mint a new fungible asset to check account vault changes
    let faucet = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await?
    .0;

    let fungible_asset = FungibleAsset::new(faucet.id(), MINT_AMOUNT)?;
    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        first_basic_account.id(),
        NoteType::Public,
        client.rng(),
    )?;
    let note = tx_request.expected_output_own_notes().pop().unwrap();
    execute_tx_and_sync(&mut client, fungible_asset.faucet_id(), tx_request.clone()).await?;

    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![note.clone()])?;
    execute_tx_and_sync(&mut client, first_basic_account.id(), tx_request).await?;

    let nullifier = note.nullifier();

    let sync_height = client.get_sync_height().await?;
    let node_nullifier = client
        .test_rpc_api()
        .sync_nullifiers(&[nullifier.prefix()], 0.into(), sync_height)
        .await
        .unwrap()
        .pop()
        .with_context(|| "no nullifier found in sync_nullifiers response")?;
    let retrieved_note_script = client
        .test_rpc_api()
        .get_note_script_by_root(note.script().root().into())
        .await
        .unwrap()
        .expect("node should have the note script registered");
    let sync_storage_maps = client
        .test_rpc_api()
        .sync_storage_maps(0.into(), sync_height, account_with_map_item.id())
        .await
        .unwrap();
    let account_vault_info = client
        .test_rpc_api()
        .sync_account_vault(0.into(), sync_height, first_basic_account.id())
        .await
        .unwrap();
    let transactions = client
        .test_rpc_api()
        .sync_transactions(0.into(), sync_height, vec![first_basic_account.id()])
        .await
        .unwrap();

    assert_eq!(node_nullifier.nullifier, nullifier);
    assert_eq!(note.script().root(), retrieved_note_script.root());
    assert!(!sync_storage_maps.updates.is_empty());
    assert!(!account_vault_info.updates.is_empty());
    assert!(!transactions.is_empty());

    Ok(())
}

pub async fn test_ignore_invalid_notes(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    let (regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await?;

    let account_id = regular_account.id();
    let second_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Mint 2 valid notes
    let (tx_id_1, note_1) =
        mint_note(&mut client, account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_1).await?;
    let (tx_id_2, note_2) =
        mint_note(&mut client, account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_2).await?;

    // Mint 2 invalid notes
    let (tx_id_3, note_3) =
        mint_note(&mut client, second_account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_3).await?;
    let (tx_id_4, note_4) =
        mint_note(&mut client, second_account_id, faucet_account_id, NoteType::Private).await;
    wait_for_tx(&mut client, tx_id_4).await?;

    // Create a transaction to consume all 4 notes but ignore the invalid ones
    let tx_request = TransactionRequestBuilder::new()
        .ignore_invalid_input_notes()
        .build_consume_notes(vec![
            note_1.clone(),
            note_3.clone(),
            note_2.clone(),
            note_4.clone(),
        ])?;

    execute_tx_and_sync(&mut client, account_id, tx_request).await?;

    // Check that only the valid notes were consumed
    let consumed_notes = client.get_input_notes(NoteFilter::Consumed).await.unwrap();
    assert_eq!(consumed_notes.len(), 2);
    assert!(consumed_notes.iter().any(|note| note.id() == Some(note_1.id())));
    assert!(consumed_notes.iter().any(|note| note.id() == Some(note_2.id())));
    Ok(())
}

pub async fn test_output_only_note(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;

    let faucet = insert_new_fungible_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap()
    .0;

    let fungible_asset = FungibleAsset::new(faucet.id(), MINT_AMOUNT).unwrap();
    let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
        fungible_asset,
        AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap(),
        NoteType::Public,
        client.rng(),
    )?;
    let note_id = tx_request.expected_output_own_notes().pop().unwrap().id();
    execute_tx_and_sync(&mut client, fungible_asset.faucet_id(), tx_request.clone()).await?;

    // The created note should be an output only note because it is not consumable by any client
    // account.
    let input_note = client.get_input_note(note_id).await.unwrap();
    assert!(input_note.is_none());

    let output_note = client.get_output_note(note_id).await.unwrap();
    assert!(output_note.is_some());
    Ok(())
}

/// Tests that `get_account` with `AccountStorageRequirements` correctly filters storage
/// map entries by key.
///
/// Creates a public account with a map slot containing 2 entries, then verifies:
/// - Requesting with empty keys returns `AllEntries` with both entries.
/// - Requesting with one specific key returns `EntriesWithProofs` with just that entry's proof.
pub async fn test_get_account_storage_map_key_filtering(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    let map_slot_name =
        StorageSlotName::new("miden::testing::client::map").expect("valid slot name");
    let map_key_1 = StorageMapKey::new(
        [Felt::from(15u32), Felt::from(15u32), Felt::from(15u32), Felt::from(15u32)].into(),
    );
    let map_value_1 =
        Word::from([Felt::from(9u32), Felt::from(12u32), Felt::from(18u32), Felt::from(30u32)]);
    let map_key_2 = StorageMapKey::new(
        [Felt::from(20u32), Felt::from(20u32), Felt::from(20u32), Felt::from(20u32)].into(),
    );
    let map_value_2 =
        Word::from([Felt::from(1u32), Felt::from(2u32), Felt::from(3u32), Felt::from(4u32)]);

    let mut storage_map = StorageMap::new();
    storage_map.insert(map_key_1, map_value_1)?;
    storage_map.insert(map_key_2, map_value_2)?;

    let map_slot = StorageSlot::with_map(map_slot_name.clone(), storage_map);
    let component_code = CodeBuilder::default()
        .compile_component_code(
            "miden::testing::map_key_filtering",
            "pub proc dummy\n push.1\n end".to_string(),
        )
        .context("failed to compile component code")?;
    let component = AccountComponent::new(
        component_code,
        vec![map_slot],
        AccountComponentMetadata::new("miden::testing::map_key_filtering"),
    )
    .map_err(|err| anyhow::anyhow!(err))?;

    let key_pair = AuthSecretKey::new_falcon512_poseidon2();
    let auth_component: AccountComponent = AuthSingleSig::new(Approver::new(
        key_pair.public_key().to_commitment(),
        AuthSchemeId::Falcon512Poseidon2,
    ))
    .into();

    let account = AccountBuilder::new(Default::default())
        .with_component(component)
        .with_auth_component(auth_component)
        .account_type(AccountType::Public)
        .build_with_schema_commitment()
        .context("failed to build account")?;
    let account_id = account.id();

    keystore.add_key(&key_pair, account_id).await.context("failed to add key")?;
    client.add_account(&account, false).await?;

    // Deploy the account (first tx updates nonce)
    let tx_id = client
        .submit_new_transaction(account_id, TransactionRequestBuilder::new().build()?)
        .await?;
    wait_for_tx(&mut client, tx_id).await?;

    let rpc = client.test_rpc_api();

    // Request all entries (empty keys)
    let requirements_all = AccountStorageRequirements::new([(map_slot_name.clone(), [].iter())]);
    let (_, proof_all) = rpc
        .get_account(
            account_id,
            GetAccountRequest {
                storage: StorageMapFetch::Slots(requirements_all),
                ..Default::default()
            },
        )
        .await?;
    let map_all = proof_all
        .find_map_details(&map_slot_name)
        .context("expected storage map details")?;

    assert!(
        matches!(map_all.entries, StorageMapEntries::AllEntries(ref e) if e.len() == 2),
        "expected AllEntries with 2 entries, got {:?}",
        map_all.entries,
    );

    // Request one specific key
    let requirements_one = AccountStorageRequirements::new([(map_slot_name.clone(), [&map_key_1])]);
    let (_, proof_one) = rpc
        .get_account(
            account_id,
            GetAccountRequest {
                storage: StorageMapFetch::Slots(requirements_one),
                ..Default::default()
            },
        )
        .await?;
    let map_one = proof_one
        .find_map_details(&map_slot_name)
        .context("expected storage map details")?;

    match &map_one.entries {
        StorageMapEntries::EntriesWithProofs(proofs) => {
            assert_eq!(proofs.len(), 1, "expected 1 proof");
            let hashed_key = map_key_1.hash().as_word();
            let value = proofs[0].get(&hashed_key);
            assert!(value.is_some(), "proof should contain the requested key");
            assert_eq!(value.unwrap(), map_value_1, "value should match the requested key's value");
        },
        other => anyhow::bail!("expected EntriesWithProofs, got {:?}", other),
    }

    Ok(())
}

/// Tests that `get_account` returns vault details based on the [`VaultFetch`] policy.
///
/// Creates a public faucet and wallet, mints tokens so the wallet holds assets, then calls
/// `get_account` three times with different vault policies:
/// - [`VaultFetch::Always`]: always fetches vault data.
/// - [`VaultFetch::IfChangedFrom`] with the current root: commitment matches the node's state, so
///   assets are empty.
/// - [`VaultFetch::Skip`] (default): vault data not requested, so assets are empty.
pub async fn test_get_account_returns_vault_details(client_config: ClientConfig) -> Result<()> {
    let (mut client, keystore) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    let (wallet, faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await?;

    // Mint tokens so the wallet has assets in its vault
    let tx_id = mint_and_consume(&mut client, wallet.id(), faucet.id(), NoteType::Public).await;
    wait_for_tx(&mut client, tx_id).await?;

    let rpc = client.test_rpc_api();

    // Query 1: VaultFetch::Always — always fetches vault data
    let (_, proof) = rpc
        .get_account(
            wallet.id(),
            GetAccountRequest {
                vault: VaultFetch::Always,
                ..Default::default()
            },
        )
        .await?;

    let (_, details) = proof.into_parts();
    let details = details.context("expected account details for public account")?;
    let vault_root = details.header.vault_root();

    assert_eq!(
        details.vault_details.assets,
        vec![Asset::Fungible(FungibleAsset::new(faucet.id(), MINT_AMOUNT).unwrap())],
        "expected exactly 1 asset (the minted fungible token)"
    );

    // Query 2: VaultFetch::IfChangedFrom(actual_root) — commitment matches, node returns empty
    // assets
    let (_, proof) = rpc
        .get_account(
            wallet.id(),
            GetAccountRequest {
                vault: VaultFetch::IfChangedFrom(vault_root),
                ..Default::default()
            },
        )
        .await?;

    let (_, details) = proof.into_parts();
    let details = details.context("expected account details for public account")?;

    assert!(
        details.vault_details.assets.is_empty(),
        "expected empty assets when vault commitment matches"
    );

    // Query 3: VaultFetch::Skip — vault data not requested, node returns empty assets
    let (_, proof) = rpc.get_account(wallet.id(), GetAccountRequest::new()).await?;

    let (_, details) = proof.into_parts();
    let details = details.context("expected account details for public account")?;

    assert!(
        details.vault_details.assets.is_empty(),
        "expected empty assets when vault commitment is not requested"
    );

    Ok(())
}

/// Tests that pruning account history removes old committed states without affecting
/// the current account state. Sets up a faucet, mints twice to build up history,
/// then verifies that `prune_account_history` deletes intermediate states while
/// keeping the account readable and unchanged.
pub async fn test_prune_account_history(client_config: ClientConfig) -> Result<()> {
    let (mut client, authenticator) = client_config.into_client().await?;
    wait_for_node(&mut client).await;

    let (basic_account, faucet_account) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    let faucet_id = faucet_account.id();
    let wallet_id = basic_account.id();

    // Mint twice: each mint advances the faucet nonce, creating historical entries.
    let (tx_id_1, _) = mint_note(&mut client, wallet_id, faucet_id, NoteType::Public).await;
    wait_for_tx(&mut client, tx_id_1).await?;

    let (tx_id_2, _) = mint_note(&mut client, wallet_id, faucet_id, NoteType::Public).await;
    wait_for_tx(&mut client, tx_id_2).await?;

    // Record faucet state before pruning.
    let faucet_before = client.get_account(faucet_id).await?.unwrap();

    // Prune faucet history up to nonce 1: should remove old committed states.
    let deleted = client.prune_account_history(faucet_id, Felt::from(1u32)).await?;
    assert!(deleted > 0, "Should have pruned old committed states");

    // Account should still be fully readable and unchanged.
    let faucet_after = client.get_account(faucet_id).await?.unwrap();
    assert_eq!(
        faucet_before.to_commitment(),
        faucet_after.to_commitment(),
        "Account state should be identical after pruning"
    );

    // Both accounts should still be readable.
    assert!(client.get_account(wallet_id).await?.is_some());
    assert!(client.get_account(faucet_id).await?.is_some());

    info!(deleted_single = deleted, "Prune account history test completed");

    Ok(())
}

//! Agglayer bridge-in and bridge-out end-to-end integration test.
//!
//! Exercises the full bridge lifecycle as network transaction flows:
//!
//! Setup & config:
//! 1. Deploy bridge and agglayer faucet accounts
//! 2. Register faucet in bridge via CONFIG_AGG_BRIDGE note
//! 3. Create and deploy destination account (basic wallet)
//!
//! Bridge-in (x2):
//! 4. Generate CLAIM proof data by calling foundry test for the destination account
//! 5. Submit UPDATE_GER note -> consumed by bridge as network transaction
//! 6. Submit CLAIM note -> consumed by bridge as network transaction, which produces MINT note
//! 7. MINT note consumed by agglayer faucet, which produces P2ID note
//! 8. Destination account consumes the resulting P2ID note
//! (Repeated twice to verify the bridge handles multiple independent claims)
//!
//! Bridge-out:
//! 9. Submit B2AGG note from destination account -> consumed by bridge as network transaction

extern crate alloc;

use alloc::vec;

use anyhow::Result;
use miden_agglayer::{
    B2AggNote,
    ClaimNote,
    ClaimNoteStorage,
    ConfigAggBridgeNote,
    ConversionMetadata,
    EthAddress,
    EthEmbeddedAccountId,
    UpdateGerNote,
    create_agglayer_faucet,
};
use miden_client::Felt;
use miden_client::account::AccountType;
use miden_client::asset::{Asset, AssetAmount, FungibleAsset};
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::crypto::FeltRng;
use miden_client::note::NoteAssets;
use miden_client::testing::common::{
    insert_new_wallet,
    wait_for_blocks,
    wait_for_consumable_notes,
    wait_for_tx,
};
use miden_client::transaction::TransactionRequestBuilder;

use super::agglayer_test_utils::generate_claim_data_for_account;
use super::{AgglayerConfig, create_agglayer_clients, setup_core_accounts};
use crate::tests::config::ClientConfig;

/// Amount of tokens to bridge out in the bridge-out phase of the test.
const BRIDGE_OUT_AMOUNT: u64 = 1000;

/// L1 (Ethereum mainnet) network ID used as the bridge-out destination.
const L1_NETWORK_ID: u32 = 0;

/// Placeholder L1 destination address for the bridge-out test.
const TEST_L1_DESTINATION: &str = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";

// BRIDGE-IN-OUT TEST
// ================================================================================================

/// Tests the full bridge-in then bridge-out flow using network transactions.
///
/// If `AGGLAYER_ACCOUNTS_DIR` is set, loads bridge admin, GER manager, bridge, and faucet
/// from genesis files. Otherwise, creates and deploys all accounts at runtime.
/// In both modes, a fresh destination account is always created for the user.
///
/// Setup & config:
/// 1. Creates bridge admin, GER manager (real wallets), bridge account, and agglayer faucet (loaded
///    from genesis or created at runtime)
/// 2. Deploys bridge and agglayer faucet on-chain (skipped in genesis mode)
/// 3. Registers faucet in bridge via CONFIG_AGG_BRIDGE note (skipped in genesis mode)
/// 4. Creates and deploys destination account (basic wallet, always fresh)
///
/// Bridge-in:
/// 5. Generates CLAIM proof data by running the foundry test for the destination account
/// 6. Submits UPDATE_GER note from GER manager -> consumed by bridge as network tx
/// 7. Submits CLAIM note from GER manager -> consumed by agglayer faucet as network tx
/// 8. Destination account consumes the resulting P2ID note to receive bridged tokens
///
/// Bridge-out:
/// 9. Destination account creates B2AGG note with bridged-in assets
/// 10. B2AGG note is consumed by bridge as a network transaction
pub async fn test_agglayer_bridge_in_out(client_config: ClientConfig) -> Result<()> {
    let agglayer_config = AgglayerConfig::from_env()?;
    let (mut bridge_admin, mut ger_manager, mut user) =
        create_agglayer_clients(&client_config).await?;
    let (bridge_admin_id, ger_manager_id, bridge_id) = setup_core_accounts(
        agglayer_config.as_ref(),
        &mut bridge_admin,
        &mut ger_manager,
        &mut user,
    )
    .await?;

    // ============================================================================================
    // SETUP: Destination account (always fresh) + faucet
    // ============================================================================================

    let (destination_account, ..) = insert_new_wallet(
        &mut user.client,
        AccountType::Public,
        &user.keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    println!("[bridge_in_out] Destination account created: {:?}", destination_account.id());

    let deploy_dest_tx = TransactionRequestBuilder::new().build()?;
    let tx_id = user
        .client
        .submit_new_transaction(destination_account.id(), deploy_dest_tx)
        .await?;
    wait_for_tx(&mut user.client, tx_id).await?;
    println!("[bridge_in_out] Destination account deployed on-chain");

    // Obtain the agglayer faucet (an `AuthNetworkAccount`). In genesis mode it is pre-deployed and
    // imported from `.mac` files; in runtime mode it is created and deployed within the test.
    let agglayer_faucet_id = match agglayer_config.as_ref() {
        Some(config) => {
            let faucet_id = config.faucet_id();
            println!("[bridge_in_out] Importing genesis faucet: {faucet_id}");
            for pair in [&mut bridge_admin, &mut ger_manager, &mut user] {
                config.import_account(faucet_id, &mut pair.client, &pair.keystore).await?;
            }
            faucet_id
        },
        None => {
            let faucet_seed = bridge_admin.client.rng().draw_word();
            let faucet = create_agglayer_faucet(
                faucet_seed,
                "AGG",
                12,
                Felt::from(1_000_000_000u32),
                bridge_id,
            );
            let faucet_id = faucet.id();
            println!("[bridge_in_out] Creating runtime faucet: {faucet_id}");
            for pair in [&mut bridge_admin, &mut ger_manager, &mut user] {
                pair.client.add_account(&faucet, false).await?;
            }
            let deploy_faucet_tx = TransactionRequestBuilder::new().build()?;
            let tx_id =
                bridge_admin.client.submit_new_transaction(faucet_id, deploy_faucet_tx).await?;
            wait_for_tx(&mut bridge_admin.client, tx_id).await?;
            println!("[bridge_in_out] Agglayer faucet deployed on-chain");
            faucet_id
        },
    };

    // Register the faucet on the (genesis-deployed, unconfigured) bridge via a CONFIG_AGG_BRIDGE
    // note.
    let (_, leaf_preview, _) = generate_claim_data_for_account(destination_account.id(), None)?;
    let origin_token_address = leaf_preview.origin_token_address;
    let origin_network = leaf_preview.origin_network;
    let metadata_hash = leaf_preview.metadata_hash;
    let scale = 0u8;

    let config_note = ConfigAggBridgeNote::create(
        ConversionMetadata {
            faucet_account_id: agglayer_faucet_id,
            origin_token_address,
            scale,
            origin_network,
            is_native: false,
            metadata_hash,
        },
        bridge_admin_id,
        bridge_id,
        bridge_admin.client.rng(),
    )?;
    let config_output_tx =
        TransactionRequestBuilder::new().own_output_notes(vec![config_note]).build()?;
    let tx_id = bridge_admin
        .client
        .submit_new_transaction(bridge_admin_id, config_output_tx)
        .await?;
    wait_for_tx(&mut bridge_admin.client, tx_id).await?;
    println!("[bridge_in_out] CONFIG_AGG_BRIDGE note submitted");

    // Wait for the bridge to consume the config note as a network transaction. In CI the node's
    // network transaction queue may be congested, so allow more blocks than the local minimum.
    wait_for_blocks(&mut bridge_admin.client, 5).await;
    println!("[bridge_in_out] Waited for bridge to consume CONFIG_AGG_BRIDGE note");

    // ============================================================================================
    // PHASE 1: BRIDGE-IN (x2) - two independent claim cycles
    // ============================================================================================

    for round in 1..=2 {
        println!("[bridge_in_out] === Bridge-in round {round} ===");

        let (proof_data, leaf_data, ger) =
            generate_claim_data_for_account(destination_account.id(), Some(&origin_token_address))?;
        println!("[bridge_in_out] Round {round}: claim data generated via foundry");

        let generated_dest_account_id =
            EthEmbeddedAccountId::try_from(leaf_data.destination_address)
                .expect("generated destination address should be a valid embedded Miden AccountId")
                .into_account_id();
        assert_eq!(
            generated_dest_account_id,
            destination_account.id(),
            "foundry-generated destination must match our wallet's AccountId"
        );

        ger_manager.client.sync_state().await?;

        // Submit UPDATE_GER note: done by the ger manager
        let update_ger_note =
            UpdateGerNote::create(ger, ger_manager_id, bridge_id, ger_manager.client.rng())?;
        let tx_request = TransactionRequestBuilder::new()
            .own_output_notes(vec![update_ger_note])
            .build()?;
        let tx_id = ger_manager.client.submit_new_transaction(ger_manager_id, tx_request).await?;
        wait_for_tx(&mut ger_manager.client, tx_id).await?;
        println!("[bridge_in_out] Round {round}: UPDATE_GER note submitted");

        wait_for_blocks(&mut ger_manager.client, 5).await;
        println!("[bridge_in_out] Round {round}: waited for bridge to consume UPDATE_GER note");

        // Submit CLAIM note: done by the user (or could also be a claim manager entity)
        let miden_claim_amount = leaf_data
            .amount
            .scale_to_token_amount(scale as u32)
            .expect("amount should scale successfully");
        println!("[bridge_in_out] Round {round}: miden claim amount: {:?}", miden_claim_amount);

        let claim_inputs = ClaimNoteStorage {
            proof_data,
            leaf_data,
            miden_claim_amount,
        };
        let claim_note = ClaimNote::create(
            claim_inputs,
            bridge_id,
            destination_account.id(),
            user.client.rng(),
        )?;
        let tx_request =
            TransactionRequestBuilder::new().own_output_notes(vec![claim_note]).build()?;
        let tx_id =
            user.client.submit_new_transaction(destination_account.id(), tx_request).await?;
        wait_for_tx(&mut user.client, tx_id).await?;
        println!("[bridge_in_out] Round {round}: CLAIM note submitted");

        // Wait for the P2ID note to arrive at the destination
        let consumable_notes =
            wait_for_consumable_notes(&mut user.client, destination_account.id(), 30).await;
        println!(
            "[bridge_in_out] Round {round}: found {} consumable notes for destination",
            consumable_notes.len()
        );

        let notes_to_consume: Vec<_> = consumable_notes
            .into_iter()
            .map(|(note, _)| note.try_into().map_err(|e| anyhow::anyhow!("{e}")))
            .collect::<Result<Vec<_>, _>>()?;
        let consume_tx = TransactionRequestBuilder::new().build_consume_notes(notes_to_consume)?;
        let tx_id =
            user.client.submit_new_transaction(destination_account.id(), consume_tx).await?;
        wait_for_tx(&mut user.client, tx_id).await?;
        println!("[bridge_in_out] Round {round}: destination consumed P2ID note");

        user.client.sync_state().await?;
        let dest_balance = user
            .client
            .account_reader(destination_account.id())
            .get_balance(agglayer_faucet_id)
            .await?;
        println!("[bridge_in_out] Round {round}: destination balance: {}", dest_balance);
        assert!(
            dest_balance > AssetAmount::ZERO,
            "destination should have positive balance after bridge-in round {round}"
        );
    }

    println!("[bridge_in_out] Both bridge-in rounds completed");

    // ============================================================================================
    // PHASE 2: BRIDGE-OUT
    // ============================================================================================

    user.client.sync_state().await?;

    let l1_destination_address =
        EthAddress::from_hex(TEST_L1_DESTINATION).expect("valid L1 destination address");

    // The bridge-in MINT note mints the agglayer faucet's asset with callbacks enabled, so the
    // destination holds it under the callbacks-enabled vault key. The bridge-out asset must carry
    // the same flag; otherwise the transaction kernel looks for the asset under the
    // callbacks-disabled key and fails to remove it from the vault.
    let bridge_asset: Asset = FungibleAsset::new(agglayer_faucet_id, BRIDGE_OUT_AMOUNT)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .into();
    let b2agg_note = B2AggNote::create(
        L1_NETWORK_ID,
        l1_destination_address,
        NoteAssets::new(vec![bridge_asset])?,
        bridge_id,
        destination_account.id(),
        user.client.rng(),
    )?;
    println!("[bridge_in_out] B2AGG note created with amount: {}", BRIDGE_OUT_AMOUNT);

    let b2agg_output_tx =
        TransactionRequestBuilder::new().own_output_notes(vec![b2agg_note]).build()?;
    let tx_id = user
        .client
        .submit_new_transaction(destination_account.id(), b2agg_output_tx)
        .await?;
    wait_for_tx(&mut user.client, tx_id).await?;
    println!("[bridge_in_out] B2AGG note submitted from destination account");

    // Wait for bridge to consume the B2AGG note as network transaction.
    // Allow extra blocks for CI where the node processes many concurrent network transactions.
    wait_for_blocks(&mut user.client, 5).await;
    println!("[bridge_in_out] Waited for bridge to consume B2AGG note");

    println!("[bridge_in_out] Test completed successfully");
    Ok(())
}

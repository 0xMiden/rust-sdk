#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use std::path::Path;
use std::time::Instant;

use miden_client::account::component::{AccountComponent, AccountComponentMetadata, BasicWallet};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountId,
    AccountType,
    StorageMap,
    StorageSlot,
    StorageSlotName,
};
use miden_client::assembly::CodeBuilder;
use miden_client::auth::{Approver, AuthSchemeId, AuthSecretKey, AuthSingleSig};
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::{Client, Serializable};
use rand::RngExt;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

use crate::generators::{SlotDescriptor, generate_reader_component_code};
use crate::masm::generate_expansion_component_code;
use crate::report::format_size;

/// Waits for the chain height to advance, ensuring transaction is in a block
pub(crate) async fn wait_for_block_advancement(
    client: &mut Client<FilesystemKeyStore>,
) -> anyhow::Result<()> {
    let initial_height = client.get_sync_height().await?;
    let target_height = initial_height.as_u32() + 1;

    loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        client.sync_state().await?;
        let current_height = client.get_sync_height().await?;
        if current_height.as_u32() >= target_height {
            break;
        }
    }

    Ok(())
}

/// Creates an account with empty storage maps, expansion procedures, and reader procedures.
fn create_account_with_empty_maps(
    num_maps: usize,
    seed: [u8; 32],
) -> anyhow::Result<(Account, AuthSecretKey)> {
    let sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut ChaCha20Rng::from_seed(seed));

    // Create empty storage map slots
    let storage_slots: Vec<StorageSlot> = (0..num_maps)
        .map(|i| {
            let slot_name = format!("miden::bench::map_slot_{i}");
            StorageSlot::with_map(
                StorageSlotName::new(slot_name.as_str()).expect("slot name should be valid"),
                StorageMap::new(),
            )
        })
        .collect();

    // Expansion component: provides set_item_slot_N procedures (needed for expand command)
    let expansion_code = generate_expansion_component_code(num_maps);
    let expansion_component_code = CodeBuilder::default()
        .compile_component_code("miden::bench::storage_expander", &expansion_code)
        .map_err(|e| anyhow::anyhow!("Failed to compile expansion component: {e}"))?;
    let expansion_component = AccountComponent::new(
        expansion_component_code,
        storage_slots,
        AccountComponentMetadata::new("miden::testing::storage_expander"),
    )
    .map_err(|e| anyhow::anyhow!("Failed to create expansion component: {e}"))?;

    // Reader component: provides get_map_item_slot_N procedures (needed for transaction benchmarks)
    let descriptors: Vec<SlotDescriptor> = (0..num_maps)
        .map(|i| SlotDescriptor {
            name: format!("miden::bench::map_slot_{i}"),
            is_map: true,
        })
        .collect();
    let reader_code = generate_reader_component_code(&descriptors);
    let reader_component_code = CodeBuilder::default()
        .compile_component_code("miden::bench::storage_reader", &reader_code)
        .map_err(|e| anyhow::anyhow!("Failed to compile reader component: {e}"))?;
    let reader_component = AccountComponent::new(
        reader_component_code,
        vec![],
        AccountComponentMetadata::new("miden::testing::storage_reader"),
    )
    .map_err(|e| anyhow::anyhow!("Failed to create reader component: {e}"))?;

    // Basic wallet for normal operations
    let wallet_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![],
        BasicWallet::component_metadata(),
    )
    .expect("basic wallet component should satisfy account component requirements");

    let account = AccountBuilder::new(seed)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            sk.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .account_type(AccountType::Public)
        .with_component(wallet_component)
        .with_component(expansion_component)
        .with_component(reader_component)
        .build_with_schema_commitment()?;

    Ok((account, sk))
}

/// Creates and deploys a public wallet with empty storage maps to the network.
/// Returns the account ID. The signing key and account data are persisted in the
/// store directory for use by subsequent `expand` and `transaction` commands.
pub async fn deploy_account(
    client: &mut Client<FilesystemKeyStore>,
    store_path: &Path,
    maps: usize,
) -> anyhow::Result<AccountId> {
    println!("Storage maps: {maps} (empty)");
    println!();

    let total_t = Instant::now();

    // Generate a random seed for the account
    let mut rng = rand::rng();
    let mut account_seed = [0u8; 32];
    rng.fill(&mut account_seed);

    // Create account with empty maps
    let t = Instant::now();
    println!("Creating account with {maps} empty storage maps...");
    let (account, secret_key) = create_account_with_empty_maps(maps, account_seed)?;
    println!("  Done in {:.2?}", t.elapsed());

    let account_id = account.id();

    // Add key to the filesystem keystore and account to the client
    let keystore_path = store_path.join("keystore");
    let keystore =
        FilesystemKeyStore::new(keystore_path).expect("Failed to create keystore handle");
    keystore.add_key(&secret_key, account_id).await?;
    client.add_account(&account, false).await?;

    // Deploy the account by submitting an empty transaction
    println!("Deploying account to network...");
    let tx_request = TransactionRequestBuilder::new().build()?;
    let deploy_t = Instant::now();

    let t = Instant::now();
    let tx_result = client.execute_transaction(account_id, tx_request).await?;
    println!("  Execute: {:.2?}", t.elapsed());

    let t = Instant::now();
    let proven_tx = client.prove_transaction(&tx_result).await?;
    let prove_elapsed = t.elapsed();
    let tx_size = proven_tx.to_bytes().len();
    println!("  Prove: {:.2?} (tx size: {})", prove_elapsed, format_size(tx_size));

    let t = Instant::now();
    let submission_height = client.submit_proven_transaction(proven_tx, &tx_result).await?;
    println!("  Submit: {:.2?}", t.elapsed());

    let t = Instant::now();
    client.apply_transaction(&tx_result, submission_height).await?;
    println!("  Apply: {:.2?}", t.elapsed());

    println!("  Total: {:.2?}", deploy_t.elapsed());

    // Wait for blocks to ensure deployment is finalized
    let t = Instant::now();
    println!("Waiting for chain block height to advance...");
    for _ in 0..4 {
        wait_for_block_advancement(client).await?;
    }
    println!("  Done in {:.2?}", t.elapsed());

    println!();
    println!("Total deploy time: {:.2?}", total_t.elapsed());
    println!();
    println!("Account ID: {account_id}");

    Ok(account_id)
}

use std::boxed::Box;
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::string::ToString;
use std::time::{Duration, Instant};
use std::vec::Vec;

use anyhow::{Context, Result};
use miden_protocol::account::auth::AuthSecretKey;
use miden_protocol::account::{Account, AccountComponentMetadata, AccountId};
use miden_protocol::asset::{AssetAmount, FungibleAsset, TokenSymbol};
use miden_protocol::note::NoteType;
use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;
use miden_protocol::transaction::TransactionId;
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::faucets::TokenName;
use miden_standards::code_builder::CodeBuilder;
use rand::Rng;
use tracing::{debug, info};
use uuid::Uuid;

use crate::account::component::{
    AccountComponent,
    BasicWallet,
    BurnPolicy,
    FungibleFaucet,
    MintPolicy,
    TokenPolicyManager,
};
use crate::account::{AccountBuilder, AccountBuilderSchemaCommitmentExt, AccountType, StorageSlot};
use crate::auth::AuthSchemeId;
use crate::crypto::FeltRng;
pub use crate::keystore::{FilesystemKeyStore, Keystore};
use crate::note::{Note, NoteConsumability, P2idNote};
use crate::rpc::RpcError;
use crate::store::{InputNoteRecord, NoteFilter, TransactionFilter};
use crate::sync::SyncSummary;
use crate::transaction::{
    NoteArgs,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionRequestError,
    TransactionStatus,
};
use crate::{Client, ClientError};

pub type TestClient = Client<FilesystemKeyStore>;

// CONSTANTS
// ================================================================================================
pub const ACCOUNT_ID_REGULAR: u128 = ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;

/// Constant that represents the number of blocks until the p2id can be recalled. If this value is
/// too low, some tests might fail due to expected recall failures not happening.
pub const RECALL_HEIGHT_DELTA: u32 = 50;

pub fn create_test_store_path() -> PathBuf {
    let mut temp_file = temp_dir();
    temp_file.push(format!("{}.sqlite3", Uuid::new_v4()));
    temp_file
}

/// Inserts a new wallet account into the client and into the keystore.
pub async fn insert_new_wallet(
    client: &mut TestClient,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
    auth_scheme: AuthSchemeId,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    insert_new_wallet_with_seed(client, visibility, keystore, init_seed, auth_scheme).await
}

/// Inserts a new wallet account built with the provided seed into the client and into the keystore.
pub async fn insert_new_wallet_with_seed(
    client: &mut TestClient,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
    init_seed: [u8; 32],
    auth_scheme: AuthSchemeId,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let key_pair = match auth_scheme {
        AuthSchemeId::Falcon512Poseidon2 => AuthSecretKey::new_falcon512_poseidon2(),
        AuthSchemeId::EcdsaK256Keccak => AuthSecretKey::new_ecdsa_k256_keccak(),
        other => panic!("unsupported auth scheme: {}", other.as_u8()),
    };
    let auth_component =
        AuthSingleSig::new(Approver::new(key_pair.public_key().to_commitment(), auth_scheme));

    let account = AccountBuilder::new(init_seed)
        .account_type(visibility)
        .with_auth_component(auth_component)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await?;

    info!(account_id = %account.id(), ?visibility, "Inserted new wallet");

    Ok((account, key_pair))
}

/// Inserts a new fungible faucet account into the client and into the keystore.
pub async fn insert_new_fungible_faucet(
    client: &mut TestClient,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
    auth_scheme: AuthSchemeId,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let key_pair = match auth_scheme {
        AuthSchemeId::Falcon512Poseidon2 => AuthSecretKey::new_falcon512_poseidon2(),
        AuthSchemeId::EcdsaK256Keccak => AuthSecretKey::new_ecdsa_k256_keccak(),
        other => panic!("unsupported auth scheme: {}", other.as_u8()),
    };
    let auth_component =
        AuthSingleSig::new(Approver::new(key_pair.public_key().to_commitment(), auth_scheme));

    // we need to use an initial seed to create the faucet account
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let symbol = TokenSymbol::new("TEST").unwrap();
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let max_supply = 9_999_999_u64;
    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(10)
        .max_supply(AssetAmount::new(max_supply).unwrap())
        .build()
        .unwrap();

    // Only mint/burn policies — registering transfer (send/receive) policies installs asset
    // callback slots on the faucet, which forces `FungibleAsset` keys to carry
    // `AssetCallbackFlag::Enabled`. Tests construct assets via `FungibleAsset::new`, which
    // defaults to `Disabled`, so adding transfer policies makes `mint_and_send` reject the
    // mint with `ERR_FUNGIBLE_MINT_NOTE_ASSET_NOT_FROM_THIS_FAUCET`.
    let policy_manager = TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::allow_all())
        .active_burn_policy(BurnPolicy::allow_all())
        .build();
    let account = AccountBuilder::new(init_seed)
        .account_type(visibility)
        .with_auth_component(auth_component)
        .with_component(faucet)
        .with_components(policy_manager)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await?;

    info!(account_id = %account.id(), ?visibility, "Inserted new fungible faucet");

    Ok((account, key_pair))
}

/// Executes a transaction and asserts that it fails with the expected error.
pub async fn execute_failing_tx(
    client: &mut TestClient,
    account_id: AccountId,
    tx_request: TransactionRequest,
    expected_error: ClientError,
) {
    info!(account_id = %account_id, "Executing transaction (expecting failure)");
    // We compare string since we can't compare the error directly
    assert_eq!(
        Box::pin(client.submit_new_transaction(account_id, tx_request))
            .await
            .unwrap_err()
            .to_string(),
        expected_error.to_string()
    );
}

/// Executes a transaction and waits for it to be committed.
pub async fn execute_tx_and_sync(
    client: &mut TestClient,
    account_id: AccountId,
    tx_request: TransactionRequest,
) -> Result<()> {
    let transaction_id = Box::pin(client.submit_new_transaction(account_id, tx_request)).await?;
    info!(tx_id = %transaction_id, account_id = %account_id, "Transaction submitted, waiting for commit");
    wait_for_tx(client, transaction_id).await?;
    Ok(())
}

/// Syncs the client and waits for the transaction to be committed.
pub async fn wait_for_tx(client: &mut TestClient, transaction_id: TransactionId) -> Result<()> {
    // wait until tx is committed
    let now = Instant::now();
    debug!(tx_id = %transaction_id, "Waiting for transaction to be committed");
    loop {
        client
            .sync_state()
            .await
            .with_context(|| "failed to sync client state while waiting for transaction")?;

        // Check if executed transaction got committed by the node
        let tracked_transaction = client
            .get_transactions(TransactionFilter::Ids(vec![transaction_id]))
            .await
            .with_context(|| format!("failed to get transaction with ID: {transaction_id}"))?
            .pop()
            .with_context(|| format!("transaction with ID {transaction_id} not found"))?;

        match tracked_transaction.status {
            TransactionStatus::Committed { block_number, .. } => {
                info!(tx_id = %transaction_id, %block_number, "Transaction committed");
                break;
            },
            TransactionStatus::Pending => {
                // Cooldown between polling iterations to reduce pressure on the node's
                // rate limiter when many integration tests poll concurrently.
                tokio::time::sleep(Duration::from_millis(500)).await;
            },
            TransactionStatus::Discarded(cause) => {
                anyhow::bail!("transaction was discarded with cause: {cause:?}");
            },
        }

        // Log wait time in a file if the env var is set
        // This allows us to aggregate and measure how long the tests are waiting for transactions
        // to be committed
        if std::env::var("LOG_WAIT_TIMES") == Ok("true".to_string()) {
            let elapsed = now.elapsed();
            let wait_times_dir = std::path::PathBuf::from("wait_times");
            std::fs::create_dir_all(&wait_times_dir)
                .with_context(|| "failed to create wait_times directory")?;

            let elapsed_time_file = wait_times_dir.join(format!("wait_time_{}", Uuid::new_v4()));
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(elapsed_time_file)
                .with_context(|| "failed to create elapsed time file")?;
            writeln!(file, "{:?}", elapsed.as_millis())
                .with_context(|| "failed to write elapsed time to file")?;
        }
    }
    Ok(())
}

/// Syncs until `amount_of_blocks` have been created onchain compared to client's sync height
pub async fn wait_for_blocks(client: &mut TestClient, amount_of_blocks: u32) -> SyncSummary {
    let current_block = client.get_sync_height().await.unwrap();
    let final_block = current_block + amount_of_blocks;
    debug!(current_block = %current_block, target_block = %final_block, "Waiting for blocks");
    loop {
        let summary = client.sync_state().await.unwrap();
        debug!(sync_height = %summary.block_num, target_block = %final_block, "Synced");

        if summary.block_num >= final_block {
            return summary;
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Idles until `amount_of_blocks` have been created onchain compared to client's sync height
/// without advancing the client's sync height
pub async fn wait_for_blocks_no_sync(client: &mut TestClient, amount_of_blocks: u32) {
    let current_block = client.get_sync_height().await.unwrap();
    let final_block = current_block + amount_of_blocks;
    debug!(current_block = %current_block, target_block = %final_block, "Waiting for blocks (no sync)");
    loop {
        let (latest_block, _) =
            client.test_rpc_api().get_block_header_by_number(None, false).await.unwrap();
        debug!(
            chain_tip = %latest_block.block_num(),
            target_block = %final_block,
            "Waiting for blocks (no sync)"
        );

        if latest_block.block_num() >= final_block {
            return;
        }

        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}

/// Syncs repeatedly until the given account has at least one consumable note, or until
/// `max_blocks` have elapsed since the call. Returns the list of consumable notes once found.
///
/// This is useful when waiting for a network transaction to produce an output note (e.g., a
/// P2ID note created by a faucet after consuming a CLAIM note), where the exact number of
/// blocks needed is unpredictable.
///
/// # Panics
///
/// Panics if `max_blocks` elapse without any consumable notes appearing.
pub async fn wait_for_consumable_notes(
    client: &mut TestClient,
    account_id: AccountId,
    max_blocks: u32,
) -> Vec<(InputNoteRecord, Vec<NoteConsumability>)> {
    let start_block = client.get_sync_height().await.unwrap();
    let deadline_block = start_block + max_blocks;
    debug!(
        %account_id,
        %start_block,
        %deadline_block,
        "Waiting for consumable notes"
    );

    loop {
        client.sync_state().await.unwrap();
        let notes = client.get_consumable_notes(Some(account_id)).await.unwrap();
        if !notes.is_empty() {
            let current_block = client.get_sync_height().await.unwrap();
            debug!(
                %account_id,
                count = notes.len(),
                %current_block,
                "Found consumable notes"
            );
            return notes;
        }

        let current_block = client.get_sync_height().await.unwrap();
        assert!(
            current_block < deadline_block,
            "account {account_id} has no consumable notes after waiting {max_blocks} blocks \
             (from block {start_block} to {current_block})"
        );

        debug!(
            %account_id,
            %current_block,
            %deadline_block,
            "No consumable notes yet, waiting..."
        );
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Waits for node to be running.
///
/// # Panics
///
/// This function will panic if it does `NUMBER_OF_NODE_ATTEMPTS` unsuccessful checks or if we
/// receive an error other than a connection related error.
pub async fn wait_for_node(client: &mut TestClient) {
    const NODE_TIME_BETWEEN_ATTEMPTS: u64 = 2;
    const NUMBER_OF_NODE_ATTEMPTS: u64 = 60;
    info!(
        "Waiting for node to be up (checking every {NODE_TIME_BETWEEN_ATTEMPTS}s, max {NUMBER_OF_NODE_ATTEMPTS} tries)"
    );
    for _try_number in 0..NUMBER_OF_NODE_ATTEMPTS {
        match client.sync_state().await {
            Err(ClientError::RpcError(
                RpcError::ConnectionError(_) | RpcError::RequestError { .. },
            )) => {
                tokio::time::sleep(Duration::from_secs(NODE_TIME_BETWEEN_ATTEMPTS)).await;
            },
            Err(other_error) => {
                panic!("Unexpected error: {other_error}");
            },
            _ => return,
        }
    }

    panic!("Unable to connect to node");
}

pub const MINT_AMOUNT: u64 = 1000;
pub const TRANSFER_AMOUNT: u64 = 59;

/// Sets up a basic client and returns two basic accounts and a faucet account (in that order).
pub async fn setup_two_wallets_and_faucet(
    client: &mut TestClient,
    account_visibility: AccountType,
    keystore: &FilesystemKeyStore,
    auth_scheme: AuthSchemeId,
) -> Result<(Account, Account, Account)> {
    // Ensure clean state
    let account_headers = client
        .get_account_headers()
        .await
        .with_context(|| "failed to get account headers")?;
    anyhow::ensure!(account_headers.is_empty(), "Expected empty account headers for clean state");

    let transactions = client
        .get_transactions(TransactionFilter::All)
        .await
        .with_context(|| "failed to get transactions")?;
    anyhow::ensure!(transactions.is_empty(), "Expected empty transactions for clean state");

    let input_notes = client
        .get_input_notes(NoteFilter::All)
        .await
        .with_context(|| "failed to get input notes")?;
    anyhow::ensure!(input_notes.is_empty(), "Expected empty input notes for clean state");

    // Create faucet account
    let (faucet_account, _) =
        insert_new_fungible_faucet(client, account_visibility, keystore, auth_scheme)
            .await
            .with_context(|| "failed to insert new fungible faucet account")?;

    // Create regular accounts
    let (first_basic_account, ..) =
        insert_new_wallet(client, account_visibility, keystore, auth_scheme)
            .await
            .with_context(|| "failed to insert first basic wallet account")?;

    let (second_basic_account, ..) =
        insert_new_wallet(client, account_visibility, keystore, auth_scheme)
            .await
            .with_context(|| "failed to insert second basic wallet account")?;

    info!(
        faucet_id = %faucet_account.id(),
        wallet_1_id = %first_basic_account.id(),
        wallet_2_id = %second_basic_account.id(),
        "Setup complete, syncing state"
    );
    client.sync_state().await.with_context(|| "failed to sync client state")?;

    Ok((first_basic_account, second_basic_account, faucet_account))
}

/// Sets up a basic client and returns a basic account and a faucet account.
pub async fn setup_wallet_and_faucet(
    client: &mut TestClient,
    account_visibility: AccountType,
    keystore: &FilesystemKeyStore,
    auth_scheme: AuthSchemeId,
) -> Result<(Account, Account)> {
    let (faucet_account, _) =
        insert_new_fungible_faucet(client, account_visibility, keystore, auth_scheme)
            .await
            .with_context(|| "failed to insert new fungible faucet account")?;

    let (basic_account, ..) = insert_new_wallet(client, account_visibility, keystore, auth_scheme)
        .await
        .with_context(|| "failed to insert new wallet account")?;

    Ok((basic_account, faucet_account))
}

/// Mints a note from `faucet_account_id` for `basic_account_id` and returns the executed
/// transaction ID and the note with [`MINT_AMOUNT`] units of the corresponding fungible asset.
pub async fn mint_note(
    client: &mut TestClient,
    basic_account_id: AccountId,
    faucet_account_id: AccountId,
    note_type: NoteType,
) -> (TransactionId, Note) {
    // Create a Mint Tx for MINT_AMOUNT units of our fungible asset
    let fungible_asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap();
    info!(faucet_id = %faucet_account_id, target_id = %basic_account_id, amount = MINT_AMOUNT, "Minting asset");
    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(fungible_asset, basic_account_id, note_type, client.rng())
        .unwrap();
    let tx_id =
        Box::pin(client.submit_new_transaction(fungible_asset.faucet_id(), tx_request.clone()))
            .await
            .unwrap();

    let note = tx_request.expected_output_own_notes().pop().unwrap();
    info!(tx_id = %tx_id, note_id = %note.id(), "Mint transaction submitted");
    (tx_id, note)
}

/// Executes a transaction that consumes the provided notes and returns the transaction ID.
/// This assumes the notes contain assets.
pub async fn consume_notes(
    client: &mut TestClient,
    account_id: AccountId,
    input_notes: &[Note],
) -> TransactionId {
    let note_ids: Vec<_> = input_notes.iter().map(|n| n.id().to_string()).collect();
    info!(account_id = %account_id, note_ids = %note_ids.join(", "), "Consuming notes");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(input_notes.to_vec())
        .unwrap();
    let tx_id = Box::pin(client.submit_new_transaction(account_id, tx_request)).await.unwrap();
    info!(tx_id = %tx_id, "Consume transaction submitted");
    tx_id
}

/// Asserts that the account has a single asset with the expected amount.
pub async fn assert_account_has_single_asset(
    client: &TestClient,
    account_id: AccountId,
    faucet_id: AccountId,
    expected_amount: u64,
) {
    let balance = client
        .account_reader(account_id)
        .get_balance(faucet_id)
        .await
        .expect("Account should have the asset");
    assert_eq!(balance, expected_amount);
}

/// Tries to consume the note and asserts that the expected error is returned.
pub async fn assert_note_cannot_be_consumed_twice(
    client: &mut TestClient,
    consuming_account_id: AccountId,
    note_to_consume: Note,
) {
    // Check that we can't consume the P2ID note again
    info!(note_id = %note_to_consume.id(), account_id = %consuming_account_id, "Attempting double-consume (expecting failure)");

    // Double-spend error expected to be received since we are consuming the same note
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note_to_consume.clone()])
        .unwrap();

    match Box::pin(client.submit_new_transaction(consuming_account_id, tx_request)).await {
        Err(ClientError::TransactionRequestError(
            TransactionRequestError::InputNoteAlreadyConsumed(_),
        )) => {},
        Ok(_) => panic!("Double-spend error: Note should not be consumable!"),
        err => panic!("Unexpected error {:?} for note ID: {}", err, note_to_consume.id().to_hex()),
    }
}

/// Creates a transaction request that mints assets for each `target_id` account.
pub fn mint_multiple_fungible_asset(
    asset: FungibleAsset,
    target_id: &[AccountId],
    note_type: NoteType,
    rng: &mut impl FeltRng,
) -> TransactionRequest {
    let notes = target_id
        .iter()
        .map(|account_id| {
            P2idNote::builder()
                .sender(asset.faucet_id())
                .target(*account_id)
                .asset(asset)
                .note_type(note_type)
                .generate_serial_number(rng)
                .build()
                .expect("note creation failed")
                .into()
        })
        .collect::<Vec<Note>>();

    TransactionRequestBuilder::new().own_output_notes(notes).build().unwrap()
}

/// Executes a transaction and consumes the resulting unauthenticated notes immediately without
/// waiting for the first transaction to be committed.
pub async fn execute_tx_and_consume_output_notes(
    tx_request: TransactionRequest,
    client: &mut TestClient,
    executor: AccountId,
    consumer: AccountId,
) -> TransactionId {
    let output_notes = tx_request
        .expected_output_own_notes()
        .into_iter()
        .map(|note| (note, None::<NoteArgs>))
        .collect::<Vec<(Note, Option<NoteArgs>)>>();

    Box::pin(client.submit_new_transaction(executor, tx_request)).await.unwrap();

    let tx_request = TransactionRequestBuilder::new().input_notes(output_notes).build().unwrap();
    Box::pin(client.submit_new_transaction(consumer, tx_request)).await.unwrap()
}

/// Mints assets for the target account and consumes them immediately without waiting for the first
/// transaction to be committed.
pub async fn mint_and_consume(
    client: &mut TestClient,
    basic_account_id: AccountId,
    faucet_account_id: AccountId,
    note_type: NoteType,
) -> TransactionId {
    info!(
        faucet_id = %faucet_account_id,
        target_id = %basic_account_id,
        amount = MINT_AMOUNT,
        "Minting and consuming asset"
    );
    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet_account_id, MINT_AMOUNT).unwrap(),
            basic_account_id,
            note_type,
            client.rng(),
        )
        .unwrap();

    let tx_id = Box::pin(execute_tx_and_consume_output_notes(
        tx_request,
        client,
        faucet_account_id,
        basic_account_id,
    ))
    .await;
    info!(tx_id = %tx_id, "Mint-and-consume transaction submitted");
    tx_id
}

/// Creates and inserts an account with custom code as a component into the client.
pub async fn insert_account_with_custom_component(
    client: &mut TestClient,
    custom_code: &str,
    storage_slots: Vec<StorageSlot>,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
) -> Result<(Account, AuthSecretKey), ClientError> {
    let component_code = CodeBuilder::default()
        .compile_component_code("custom::component", custom_code)
        .map_err(|err| ClientError::TransactionRequestError(err.into()))?;
    let custom_component = AccountComponent::new(
        component_code,
        storage_slots,
        AccountComponentMetadata::new("miden::testing::custom_component"),
    )
    .map_err(ClientError::AccountError)?;

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let pub_key = key_pair.public_key();

    let account = AccountBuilder::new(init_seed)
        .account_type(visibility)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(BasicWallet)
        .with_component(custom_component)
        .build_with_schema_commitment()
        .map_err(ClientError::AccountError)?;

    keystore.add_key(&key_pair, account.id()).await.unwrap();
    client.add_account(&account, false).await?;

    Ok((account, key_pair))
}

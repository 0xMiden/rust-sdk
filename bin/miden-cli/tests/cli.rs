use std::env::{self, temp_dir};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use assert_cmd::Command;
use assert_cmd::cargo::cargo_bin_cmd;
use miden_client::account::component::FungibleFaucet;
use miden_client::account::{AccountId, AccountType, FaucetMetadata};
use miden_client::address::{Address, NetworkId};
use miden_client::auth::{RPO_FALCON_SCHEME_ID, TransactionAuthenticator};
use miden_client::builder::ClientBuilder;
use miden_client::crypto::{FeltRng, RandomCoin};
use miden_client::keystore::Keystore;
use miden_client::note::{
    Note,
    NoteAssets,
    NoteFile,
    NoteId,
    NoteRecipient,
    NoteStorage,
    NoteTag,
    NoteType,
    PartialNoteMetadata,
};
use miden_client::note_transport::NOTE_TRANSPORT_TESTNET_ENDPOINT;
use miden_client::rpc::Endpoint;
use miden_client::testing::account_id::ACCOUNT_ID_PRIVATE_SENDER;
use miden_client::testing::common::{
    ACCOUNT_ID_REGULAR,
    FilesystemKeyStore,
    create_test_store_path,
    execute_tx_and_sync,
    insert_new_wallet,
};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::utils::Serializable;
use miden_client::{self, Client, DebugMode, Felt};
use miden_client_cli::MIDEN_DIR;
use miden_client_cli::config::Network;
use miden_client_sqlite_store::SqliteStore;
use predicates::str::contains;
use rand::Rng;

// CLI TESTS
// ================================================================================================

/// This Module contains integration tests that test against the miden CLI directly. In order to do
/// that we use [assert_cmd](https://github.com/assert-rs/assert_cmd?tab=readme-ov-file) which aids
/// in the process of spawning commands.
///
/// Tests added here should only interact with the CLI through `assert_cmd`, with the exception of
/// reading data from the client's store since it would be quite tedious to parse the CLI output
/// for that and is more error prone.
///
/// Note that each client has to run in its own directory so you'll need to create a random
/// temporary directory (check existing tests to see how). You'll also need to make the commands
/// run as if they were spawned on that directory. `std::env::set_current_dir` shouldn't be used as
/// it impacts on other tests and instead you should use `assert_cmd::Command::current_dir`.

// INIT TESTS
// ================================================================================================

#[test]
fn init_without_params() {
    let temp_dir = init_cli().1;

    // Trying to init twice should result in an error
    let mut init_cmd = cargo_bin_cmd!("miden-client");
    init_cmd.args(["init", "--local"]);
    init_cmd.current_dir(&temp_dir).assert().failure();
}

#[test]
fn init_with_params() {
    let store_path = create_test_store_path();
    let endpoint = Endpoint::devnet();
    let temp_dir = init_cli_with_store_path(&store_path, &endpoint);

    // Assert the config file contains the specified contents
    let mut config_path = temp_dir.clone();
    config_path.push(MIDEN_DIR);
    config_path.push("miden-client.toml");
    let mut config_file = File::open(config_path).unwrap();
    let mut config_file_str = String::new();
    config_file.read_to_string(&mut config_file_str).unwrap();

    assert!(config_file_str.contains(store_path.to_str().unwrap()));
    assert!(config_file_str.contains("devnet"));

    // Trying to init twice should result in an error
    let mut init_cmd = cargo_bin_cmd!("miden-client");
    init_cmd.args([
        "init",
        "--local",
        "--network",
        "devnet",
        "--store-path",
        store_path.to_str().unwrap(),
    ]);
    init_cmd.current_dir(&temp_dir).assert().failure();
}

#[test]
#[serial_test::file_serial]
fn silent_initialization_uses_default_values() {
    let miden_home = set_isolated_miden_home();

    let temp_dir = temp_dir().join(format!("cli-test-{}", rand::rng().random::<u64>()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Run any command to trigger silent initialization (should create global config)
    let mut account_cmd = cargo_bin_cmd!("miden-client");
    account_cmd.args(["account"]);
    account_cmd.current_dir(&temp_dir).assert().success();

    // Read and verify the global config file contents
    let global_config_path = miden_home.join("miden-client.toml");
    let config_content = std::fs::read_to_string(&global_config_path).unwrap();

    // Verify default values are used
    assert!(config_content.contains("testnet"), "Should use testnet as default network");
    assert!(
        config_content.contains("store.sqlite3"),
        "Should use default store path (relative to config file)"
    );
    assert!(
        config_content.contains("keystore"),
        "Should use default keystore directory (relative to config file)"
    );
    // Verify note transport defaults to the testnet endpoint
    assert!(
        config_content.contains("[note_transport]"),
        "Silent init should write a [note_transport] section"
    );
    assert!(
        config_content.contains(NOTE_TRANSPORT_TESTNET_ENDPOINT),
        "Silent init should default note transport to the testnet endpoint"
    );
    // Verify that the paths don't have the .miden prefix in the config
    // (they're relative to the config file location now)
    assert!(
        !config_content.contains(&format!("{MIDEN_DIR}/store.sqlite3")),
        "Paths should be relative to config file, not include {MIDEN_DIR}/ prefix"
    );

    // Verify no local config was created
    let local_config_path = temp_dir.join(MIDEN_DIR).join("miden-client.toml");
    assert!(
        !local_config_path.exists(),
        "Should not create local config during silent initialization"
    );
}

#[test]
fn miden_directory_structure_creation() {
    let temp_dir = temp_dir().join(format!("cli-test-{}", rand::rng().random::<u64>()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Run init command to create .miden directory structure
    let mut init_cmd = cargo_bin_cmd!("miden-client");
    init_cmd.args(["init", "--local"]);
    init_cmd.current_dir(&temp_dir).assert().success();

    let miden_dir = temp_dir.join(MIDEN_DIR);

    // Verify .miden directory exists
    assert!(miden_dir.exists(), ".miden directory should be created");
    assert!(miden_dir.is_dir(), ".miden should be a directory");

    // Verify expected files that are created during init
    let config_file = miden_dir.join("miden-client.toml");
    assert!(config_file.exists(), "config file should be created");
    assert!(config_file.is_file(), "config should be a file");

    // Verify packages directory is created with template files
    let packages_dir = miden_dir.join("packages");
    assert!(packages_dir.exists(), "packages directory should be created");
    assert!(packages_dir.is_dir(), "packages should be a directory");

    // Check that expected package files exist
    let basic_wallet_package = packages_dir.join("basic-wallet.masp");
    assert!(basic_wallet_package.exists(), "basic-wallet package should be created");

    let basic_auth_package = packages_dir.join("auth/basic-auth.masp");
    assert!(basic_auth_package.exists(), "basic-auth package should be created");

    let ecdsa_auth_package = packages_dir.join("auth/ecdsa-auth.masp");
    assert!(ecdsa_auth_package.exists(), "ecdsa-auth package should be created");

    let basic_faucet_package = packages_dir.join("basic-fungible-faucet.masp");
    assert!(basic_faucet_package.exists(), "basic-fungible-faucet package should be created");

    // Verify config file contains correct paths relative to config file location
    let config_content = std::fs::read_to_string(&config_file).unwrap();
    assert!(
        config_content.contains("store.sqlite3"),
        "Config should reference store path relative to config file"
    );
    assert!(
        config_content.contains("keystore"),
        "Config should reference keystore path relative to config file"
    );
    assert!(
        config_content.contains("packages"),
        "Config should reference packages path relative to config file"
    );
    assert!(
        config_content.contains("token_symbol_map.toml"),
        "Config should reference token symbol map path relative to config file"
    );
    // Verify that the paths don't have the .miden prefix (they're relative to config file now)
    assert!(
        !config_content.contains(&format!("{MIDEN_DIR}/store.sqlite3")),
        "Paths should be relative to config file, not include {MIDEN_DIR}/ prefix"
    );

    // Verify default RPC endpoint is set
    assert!(
        config_content.contains("https://rpc.testnet.miden.io"),
        "Config should have default testnet RPC endpoint"
    );

    // Test that keystore directory doesn't exist initially (created on demand)
    let keystore_dir = miden_dir.join("keystore");
    assert!(!keystore_dir.exists(), "keystore directory should not exist until first use");

    // Test that token symbol map file doesn't exist initially (created on demand)
    let token_map_file = miden_dir.join("token_symbol_map.toml");
    assert!(!token_map_file.exists(), "token symbol map should not exist until first use");

    // Test that running any command after init creates keystore directory on-demand
    let mut account_cmd = cargo_bin_cmd!("miden-client");
    account_cmd.args(["account"]);
    account_cmd.current_dir(&temp_dir).assert().success();

    // Now keystore directory should exist
    let keystore_dir = miden_dir.join("keystore");
    assert!(keystore_dir.exists(), "keystore directory should be created on first use");
    assert!(keystore_dir.is_dir(), "keystore should be a directory");
}

#[test]
fn silent_initialization_does_not_override_existing_config() {
    let temp_dir = temp_dir().join(format!("cli-test-{}", rand::rng().random::<u64>()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Create the MIDEN_DIR directory and manual configuration file
    let miden_dir = temp_dir.join(MIDEN_DIR);
    std::fs::create_dir_all(&miden_dir).unwrap();
    let config_path = miden_dir.join("miden-client.toml");
    // Manual configuration file
    let custom_config = format!(
        r#"
        store_filepath = "{MIDEN_DIR}/custom-store.sqlite3"
        secret_keys_directory = "{MIDEN_DIR}/custom-keystore"
        token_symbol_map_filepath = "{MIDEN_DIR}/custom-tokens.toml"
        package_directory = "{MIDEN_DIR}/custom-templates"

        [rpc]
        endpoint = "https://custom-endpoint.com"
        timeout_ms = 5000

        [remote_prover_timeout]
        secs = 20
        nanos = 0
        "#
    );
    std::fs::write(&config_path, custom_config).unwrap();

    // Run command without explicitly initializing
    let mut account_cmd = cargo_bin_cmd!("miden-client");
    account_cmd.args(["account"]);
    account_cmd.current_dir(&temp_dir).assert().success();

    // Verify original config remains unchanged
    let config_content = std::fs::read_to_string(&config_path).unwrap();
    assert!(
        config_content.contains("custom-endpoint.com"),
        "Config should not be overwritten"
    );
    assert!(
        config_content.contains("custom-store.sqlite3"),
        "Config should not be overwritten"
    );
}

// TX TESTS
// ================================================================================================

/// This test tries to run a mint TX using the CLI for an account that isn't tracked.
#[tokio::test]
async fn mint_with_untracked_account() -> Result<()> {
    let temp_dir = init_cli().1;

    // Create faucet account
    let fungible_faucet_account_id = new_faucet_cli(&temp_dir, AccountType::Private);

    sync_cli(&temp_dir);

    // Let's try and mint
    mint_cli(
        &temp_dir,
        &AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap().to_hex(),
        &fungible_faucet_account_id,
    );

    // Wait until the faucet's mint transaction is committed on the node.
    // We sync for a committed transaction (not note) because the target account is untracked,
    // so the output note's tag won't be requested during sync and the note will never appear.
    sync_until_committed_transaction(&temp_dir);
    Ok(())
}

/// This test tries to run a mint TX using the CLI for an account that isn't tracked.
#[tokio::test]
async fn token_symbol_mapping() -> Result<()> {
    let (store_path, temp_dir, endpoint) = init_cli();

    // Create faucet account
    let fungible_faucet_account_id = new_faucet_cli(&temp_dir, AccountType::Private);

    // Encode the faucet ID as bech32 using the same NetworkId the CLI derives from its
    // configured endpoint. The token symbol map's `id` field accepts bech32 only.
    let faucet_id = AccountId::from_hex(&fungible_faucet_account_id).unwrap();
    let bech32_id = Address::new(faucet_id).encode(endpoint.to_network_id());

    // Create a token symbol mapping file in the MIDEN_DIR directory
    let token_symbol_map_path = temp_dir.join(MIDEN_DIR).join("token_symbol_map.toml");
    let token_symbol_map_content = format!(r#"BTC = {{ id = "{bech32_id}", decimals = 10 }}"#);
    fs::write(&token_symbol_map_path, token_symbol_map_content).unwrap();

    sync_cli(&temp_dir);

    let mut mint_cmd = cargo_bin_cmd!("miden-client");
    mint_cmd.args([
        "mint",
        "--target",
        AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap().to_hex().as_str(),
        "--asset",
        "0.00001::BTC",
        "-n",
        "private",
        "--force",
    ]);

    let output = mint_cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        output.status.success(),
        "token_symbol mint failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let note_id = String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .skip_while(|&word| word != "Output")
        .find(|word| word.starts_with("0x"))
        .unwrap()
        .to_string();

    let note = {
        let (client, _) = create_rust_client_with_store_path(&store_path, endpoint).await?;
        client.get_output_note(NoteId::try_from_hex(&note_id)?).await?.unwrap()
    };

    assert_eq!(note.assets().num_assets(), 1);
    assert_eq!(
        note.assets().iter().next().unwrap().unwrap_fungible().amount().as_u64(),
        100_000
    );
    Ok(())
}

/// Exercises the resolver's RPC fetch + settings-store write-back path end-to-end.
///
/// Mints from a *public* faucet that is not present in the user's TOML map, then runs
/// `notes -s <id>` to display the issued note. `notes -s` formats each fungible asset via
/// `FaucetMetadataResolver::format_fungible_asset`, which on TOML miss falls through to
/// `Client::fetch_remote_token_metadata`. Asserts:
/// 1. `notes -s` stdout contains the faucet's symbol ("BTC" — the constant baked into
///    `new_faucet_cli`'s init storage data).
/// 2. After the display, the settings store contains a persisted entry for the faucet, proving the
///    resolver wrote back its RPC result.
#[tokio::test]
async fn public_faucet_metadata_is_fetched_and_persisted() -> Result<()> {
    let (store_path, temp_dir, endpoint) = init_cli();

    let wallet_account_id = new_wallet_cli(&temp_dir, AccountType::Public);
    let fungible_faucet_account_id = new_faucet_cli(&temp_dir, AccountType::Public);

    // Deliberately do NOT write a token_symbol_map.toml — the TOML path must miss so the
    // resolver falls through to the settings store and then to RPC.

    sync_cli(&temp_dir);

    // Mint from the public faucet to the wallet. The mint stdout itself does NOT route the
    // asset through the resolver (the faucet's vault delta is empty during a mint), so we
    // only use this step to obtain a valid note id.
    let mut mint_cmd = cargo_bin_cmd!("miden-client");
    mint_cmd.args([
        "mint",
        "--target",
        wallet_account_id.as_str(),
        "--asset",
        format!("100::{fungible_faucet_account_id}").as_str(),
        "-n",
        "private",
        "--force",
    ]);

    let mint_output = mint_cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        mint_output.status.success(),
        "mint failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&mint_output.stdout),
        String::from_utf8_lossy(&mint_output.stderr)
    );

    let note_id = String::from_utf8(mint_output.stdout)
        .unwrap()
        .split_whitespace()
        .skip_while(|&word| word != "Output")
        .find(|word| word.starts_with("0x"))
        .unwrap()
        .to_string();

    // Wait for the mint transaction to commit. A public faucet only becomes visible to
    // `get_account_details` once it has participated in a committed transaction.
    sync_until_committed_transaction(&temp_dir);

    // Display the note. `notes -s` formats each fungible asset via the resolver; with the
    // TOML empty and the settings store cold, the resolver must hit RPC to get ("BTC", 10) and
    // persist the result back to the settings store.
    let mut show_cmd = cargo_bin_cmd!("miden-client");
    show_cmd.args(["notes", "-s", &note_id]);
    let show_output = show_cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        show_output.status.success(),
        "notes -s failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&show_output.stdout),
        String::from_utf8_lossy(&show_output.stderr)
    );

    let show_stdout = String::from_utf8(show_output.stdout).unwrap();
    assert!(
        show_stdout.contains("BTC"),
        "expected `notes -s` stdout to contain `BTC` (faucet symbol fetched via RPC), got:\n{show_stdout}",
    );

    // Assert the resolver wrote the metadata into the settings store.
    let faucet_id = AccountId::from_hex(&fungible_faucet_account_id).unwrap();
    let (client, _) = create_rust_client_with_store_path(&store_path, endpoint).await?;
    let setting_key = format!("faucet_metadata:{}", faucet_id.to_hex());
    let stored: Option<FaucetMetadata> = client.get_setting(setting_key).await?;
    assert!(
        stored.is_some(),
        "expected settings store to contain metadata for {fungible_faucet_account_id} after notes -s",
    );
    let stored = stored.unwrap();
    assert_eq!(stored.symbol, "BTC");
    assert_eq!(stored.decimals, 10);
    Ok(())
}

// ACCOUNT SHOW TESTS
// ================================================================================================

/// Runs `account show` against a public account that is NOT tracked by the local client. The
/// account must be fetched from the node, its token metadata read from the fetched `Account`
/// storage, and its bech32 address rendered without hitting the client's store.
#[tokio::test]
async fn show_untracked_public_account() -> Result<()> {
    // First client: creates a public fungible faucet and commits it to the node via a mint.
    let (_store_path_a, temp_dir_a, endpoint) = init_cli();
    let fungible_faucet_account_id = new_faucet_cli(&temp_dir_a, AccountType::Public);
    sync_cli(&temp_dir_a);

    mint_cli(
        &temp_dir_a,
        &AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap().to_hex(),
        &fungible_faucet_account_id,
    );
    sync_until_committed_transaction(&temp_dir_a);

    // Second client: fresh CLI on the same network, not tracking the faucet.
    let store_path_b = create_test_store_path();
    let temp_dir_b = init_cli_with_store_path(&store_path_b, &endpoint);

    let mut show_cmd = cargo_bin_cmd!("miden-client");
    show_cmd.args(["account", "--show", &fungible_faucet_account_id]);
    show_cmd
        .current_dir(&temp_dir_b)
        .assert()
        .success()
        .stdout(contains("Fetching from the network"))
        .stdout(contains("Fungible faucet (token symbol: BTC)"));

    Ok(())
}

// IMPORT TESTS
// ================================================================================================

// Only one faucet is being created on the genesis block
const GENESIS_ACCOUNTS_FILENAMES: [&str; 1] = ["account.mac"];

// This tests that it's possible to import the genesis accounts and interact with them. To do so it:
//
// 1. Creates a new client
// 2. Imports the genesis account
// 3. Creates a wallet
// 4. Runs a mint tx and syncs until the transaction and note are committed
#[tokio::test]
#[ignore = "import genesis test gets ignored by default so integration tests can be ran with dockerized and remote nodes where we might not have the genesis data"]
async fn import_genesis_accounts_can_be_used_for_transactions() -> Result<()> {
    let (store_path, temp_dir, endpoint) = init_cli();

    for genesis_account_filename in GENESIS_ACCOUNTS_FILENAMES {
        let mut new_file_path = temp_dir.clone();
        new_file_path.push(genesis_account_filename);

        let cargo_workspace_dir =
            env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set");
        let source_path = format!("{cargo_workspace_dir}/../../data/{genesis_account_filename}");

        std::fs::copy(source_path, new_file_path).unwrap();
    }

    // Import genesis accounts
    let mut args = vec!["import"];
    for filename in GENESIS_ACCOUNTS_FILENAMES {
        args.push(filename);
    }
    let mut import_cmd = cargo_bin_cmd!("miden-client");
    import_cmd.args(&args);
    import_cmd.current_dir(&temp_dir).assert().success();

    sync_cli(&temp_dir);

    let fungible_faucet_account_id = {
        let (client, _) = create_rust_client_with_store_path(&store_path, endpoint).await?;
        let accounts = client.get_account_headers().await?;

        let mut faucet_accounts = Vec::new();
        for (account_header, _) in accounts {
            if let Some(account) = client.get_account(account_header.id()).await?
                && FungibleFaucet::try_from(&account).is_ok()
            {
                faucet_accounts.push(account.id());
            }
        }

        assert_eq!(faucet_accounts.len(), 1);

        faucet_accounts[0].to_hex()
    };

    // Ensure they've been importing by showing them
    let args = vec!["account", "--show", &fungible_faucet_account_id];
    let mut show_cmd = cargo_bin_cmd!("miden-client");
    show_cmd.args(&args);
    show_cmd.current_dir(&temp_dir).assert().success();

    // Let's try and mint
    mint_cli(
        &temp_dir,
        &AccountId::try_from(ACCOUNT_ID_PRIVATE_SENDER).unwrap().to_hex(),
        &fungible_faucet_account_id,
    );

    // Wait until the mint transaction is committed on the node.
    // We sync for a committed transaction (not note) because the target account is untracked.
    sync_until_committed_transaction(&temp_dir);
    Ok(())
}

// This tests that it's possible to export and import notes into other CLIs. To do so it:
//
// 1. Creates a client A with a faucet
// 2. Creates a client B with a regular account
// 3. On client A runs a mint transaction, and exports the output note
// 4. On client B imports the note and consumes it
#[tokio::test]
async fn cli_export_import_note() -> Result<()> {
    const NOTE_FILENAME: &str = "test_note.mno";

    let temp_dir_1 = init_cli().1;
    let temp_dir_2 = init_cli().1;

    // Create wallet account
    let first_basic_account_id = new_wallet_cli(&temp_dir_2, AccountType::Private);

    // Create faucet account
    let fungible_faucet_account_id = new_faucet_cli(&temp_dir_1, AccountType::Private);

    sync_cli(&temp_dir_1);

    // Let's try and mint
    let note_to_export_id =
        mint_cli(&temp_dir_1, &first_basic_account_id, &fungible_faucet_account_id);

    // Export without type fails
    let mut export_cmd = cargo_bin_cmd!("miden-client");
    export_cmd.args(["export", &note_to_export_id, "--filename", NOTE_FILENAME]);
    export_cmd.current_dir(&temp_dir_1).assert().failure().code(1); // Code returned when the CLI handles an error

    // Export the note
    let mut export_cmd = cargo_bin_cmd!("miden-client");
    export_cmd.args([
        "export",
        &note_to_export_id,
        "--filename",
        NOTE_FILENAME,
        "--export-type",
        "partial",
    ]);
    export_cmd.current_dir(&temp_dir_1).assert().success();

    // Copy the note
    let mut client_1_note_file_path = temp_dir_1.clone();
    client_1_note_file_path.push(NOTE_FILENAME);
    let mut client_2_note_file_path = temp_dir_2.clone();
    client_2_note_file_path.push(NOTE_FILENAME);
    std::fs::copy(client_1_note_file_path, client_2_note_file_path).unwrap();

    // Import Note on second client
    let mut import_cmd = cargo_bin_cmd!("miden-client");
    import_cmd.args(["import", NOTE_FILENAME]);
    import_cmd.current_dir(&temp_dir_2).assert().success();

    // Wait until the note is committed on the node
    sync_until_committed_note(&temp_dir_2);

    show_note_cli(&temp_dir_2, &note_to_export_id, false);
    // Consume the note
    consume_note_cli(&temp_dir_2, &first_basic_account_id, &[&note_to_export_id]);

    // Test send command
    let mock_target_id: AccountId = AccountId::try_from(ACCOUNT_ID_PRIVATE_SENDER).unwrap();
    send_cli(
        &temp_dir_2,
        &first_basic_account_id,
        &mock_target_id.to_hex(),
        &fungible_faucet_account_id,
    );

    Ok(())
}

#[tokio::test]
async fn cli_export_import_account() -> Result<()> {
    const FAUCET_FILENAME: &str = "test_faucet.mac";
    const WALLET_FILENAME: &str = "test_wallet.wal";

    let (_, temp_dir_1, _) = init_cli();
    let (store_path_2, temp_dir_2, endpoint_2) = init_cli();

    // Create faucet account
    let faucet_id = new_faucet_cli(&temp_dir_1, AccountType::Private);

    // Create wallet account
    let wallet_id = new_wallet_cli(&temp_dir_1, AccountType::Private);

    // Export the accounts
    let mut export_cmd = cargo_bin_cmd!("miden-client");
    export_cmd.args(["export", &faucet_id, "--account", "--filename", FAUCET_FILENAME]);
    export_cmd.current_dir(&temp_dir_1).assert().success();
    let mut export_cmd = cargo_bin_cmd!("miden-client");
    export_cmd.args(["export", &wallet_id, "--account", "--filename", WALLET_FILENAME]);
    export_cmd.current_dir(&temp_dir_1).assert().success();

    // Copy the account files
    for filename in &[FAUCET_FILENAME, WALLET_FILENAME] {
        let mut client_1_file_path = temp_dir_1.clone();
        client_1_file_path.push(filename);
        let mut client_2_file_path = temp_dir_2.clone();
        client_2_file_path.push(filename);
        std::fs::copy(client_1_file_path, client_2_file_path).unwrap();
    }

    // Import the account from the second client
    let mut import_cmd = cargo_bin_cmd!("miden-client");
    import_cmd.args(["import", FAUCET_FILENAME]);
    import_cmd.current_dir(&temp_dir_2).assert().success();
    let mut import_cmd = cargo_bin_cmd!("miden-client");
    import_cmd.args(["import", WALLET_FILENAME]);
    import_cmd.current_dir(&temp_dir_2).assert().success();

    // Ensure the account was imported
    let (client_2, _) = create_rust_client_with_store_path(&store_path_2, endpoint_2).await?;
    let cli_keystore =
        FilesystemKeyStore::new(temp_dir_2.clone().join(MIDEN_DIR).join("keystore"))?;

    assert!(client_2.get_account(AccountId::from_hex(&faucet_id)?).await.is_ok());
    assert!(client_2.get_account(AccountId::from_hex(&wallet_id)?).await.is_ok());
    sync_cli(&temp_dir_2);

    let note_id = mint_cli(&temp_dir_2, &wallet_id, &faucet_id);

    // Wait until the note is committed on the node
    sync_until_committed_note(&temp_dir_2);

    // Consume the note
    consume_note_cli(&temp_dir_2, &wallet_id, &[&note_id]);

    // Since importing keys should also store a mapping from
    // the account id to its public key commitments, we should be able
    // to retrieve them via the Keystore trait.
    let faucet_pks = cli_keystore
        .get_account_key_commitments(&AccountId::from_hex(&faucet_id)?)
        .await?;

    for stored_pk_commitment in faucet_pks {
        let matching_secret_key = cli_keystore.get_key_sync(stored_pk_commitment).unwrap();
        assert!(matching_secret_key.is_some());
        assert_eq!(matching_secret_key.unwrap().public_key().to_commitment(), stored_pk_commitment);

        let public_key = cli_keystore.get_public_key(stored_pk_commitment).await;
        assert!(public_key.is_some());
        assert_eq!(public_key.unwrap().to_commitment(), stored_pk_commitment);
    }

    let wallet_pks = cli_keystore
        .get_account_key_commitments(&AccountId::from_hex(&wallet_id)?)
        .await?;

    for stored_pk_commitment in wallet_pks {
        let matching_secret_key = cli_keystore.get_key_sync(stored_pk_commitment).unwrap();
        assert!(matching_secret_key.is_some());
        assert_eq!(matching_secret_key.unwrap().public_key().to_commitment(), stored_pk_commitment);

        let public_key = cli_keystore.get_public_key(stored_pk_commitment).await;
        assert!(public_key.is_some());
        assert_eq!(public_key.unwrap().to_commitment(), stored_pk_commitment);
    }

    Ok(())
}

#[test]
fn cli_empty_commands() {
    let temp_dir = init_cli().1;

    let mut create_faucet_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(
        create_faucet_cmd.args(["new-account"]).current_dir(&temp_dir),
    );

    let mut import_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(import_cmd.args(["export"]).current_dir(&temp_dir));

    let mut mint_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(mint_cmd.args(["mint"]).current_dir(&temp_dir));

    let mut send_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(send_cmd.args(["send"]).current_dir(&temp_dir));

    let mut swam_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(swam_cmd.args(["swap"]).current_dir(&temp_dir));

    // pswap with no subcommand should fail
    let mut pswap_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(pswap_cmd.args(["pswap"]).current_dir(&temp_dir));

    // pswap create with no args should fail
    let mut pswap_create_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(
        pswap_create_cmd.args(["pswap", "create"]).current_dir(&temp_dir),
    );

    // pswap consume with no args should fail
    let mut pswap_consume_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(
        pswap_consume_cmd.args(["pswap", "consume"]).current_dir(&temp_dir),
    );

    // pswap cancel with no args should fail
    let mut pswap_cancel_cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(
        pswap_cancel_cmd.args(["pswap", "cancel"]).current_dir(&temp_dir),
    );

    // unknown subcommand should fail
    let mut cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(cmd.args(["pswap", "unknown"]).current_dir(&temp_dir));
}

#[test]
fn pswap_cli_help_output() {
    let temp_dir = init_cli().1;

    // `pswap --help` should succeed and list subcommands
    let mut cmd = cargo_bin_cmd!("miden-client");
    let output = cmd.args(["pswap", "--help"]).current_dir(&temp_dir).output().unwrap();
    assert!(output.status.success(), "pswap --help should succeed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("create"), "Help should list 'create' subcommand");
    assert!(stdout.contains("consume"), "Help should list 'consume' subcommand");
    assert!(stdout.contains("cancel"), "Help should list 'cancel' subcommand");

    // `pswap create --help` should succeed and show flag names
    let mut cmd = cargo_bin_cmd!("miden-client");
    let output = cmd.args(["pswap", "create", "--help"]).current_dir(&temp_dir).output().unwrap();
    assert!(output.status.success(), "pswap create --help should succeed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--sender"), "Help should show --sender flag");
    assert!(stdout.contains("--offered-asset"), "Help should show --offered-asset flag");
    assert!(stdout.contains("--requested-asset"), "Help should show --requested-asset flag");
    assert!(stdout.contains("--note-type"), "Help should show --note-type flag");

    // `pswap consume --help` should show --account and --fill-amount
    let mut cmd = cargo_bin_cmd!("miden-client");
    let output = cmd
        .args(["pswap", "consume", "--help"])
        .current_dir(&temp_dir)
        .output()
        .unwrap();
    assert!(output.status.success(), "pswap consume --help should succeed");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("--account"), "Help should show --account flag");
    assert!(stdout.contains("--fill-amount"), "Help should show --fill-amount flag");
}

#[test]
fn pswap_cli_invalid_args() {
    let temp_dir = init_cli().1;

    // Required flags missing (both --offered-asset and --requested-asset are required;
    // omitting one must fail at clap parse time, before reaching `parse_fungible_asset`).
    let mut cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(
        cmd.args([
            "pswap",
            "create",
            "--sender",
            "0xaabbccdd",
            "--offered-asset",
            "100::0x1111111111111111",
            "--note-type",
            "public",
        ])
        .current_dir(&temp_dir),
    );

    // Invalid note-type
    let mut cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(
        cmd.args([
            "pswap",
            "create",
            "--sender",
            "0xaabbccdd",
            "--offered-asset",
            "100::0x1111111111111111",
            "--requested-asset",
            "50::0x2222222222222222",
            "--note-type",
            "invalid",
        ])
        .current_dir(&temp_dir),
    );

    // Invalid fill-amount for consume
    let mut cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(
        cmd.args([
            "pswap",
            "consume",
            "--account",
            "0xaabbccdd",
            "--note",
            "0xdeadbeef",
            "--fill-amount",
            "not_a_number",
        ])
        .current_dir(&temp_dir),
    );
}

#[tokio::test]
async fn consume_unauthenticated_note() -> Result<()> {
    let temp_dir = init_cli().1;

    // Create wallet account
    let wallet_account_id = new_wallet_cli(&temp_dir, AccountType::Public);

    // Create faucet account
    let fungible_faucet_account_id = new_faucet_cli(&temp_dir, AccountType::Public);

    sync_cli(&temp_dir);

    // Mint
    let note_id = mint_cli(&temp_dir, &wallet_account_id, &fungible_faucet_account_id);

    // Wait for the mint transaction to be committed on the node
    sync_until_committed_transaction(&temp_dir);

    // Consume the note, internally this checks that the note was consumed correctly
    consume_note_cli(&temp_dir, &wallet_account_id, &[&note_id]);
    Ok(())
}

// DEVNET & TESTNET TESTS
// ================================================================================================

#[tokio::test]
async fn init_with_devnet() -> Result<()> {
    let store_path = create_test_store_path();
    let endpoint = Endpoint::devnet();
    let temp_dir = init_cli_with_store_path(&store_path, &endpoint);

    // Check in the config file that the network is devnet
    let mut config_path = temp_dir.clone();
    config_path.push(MIDEN_DIR);
    config_path.push("miden-client.toml");
    let mut config_file = File::open(config_path).unwrap();
    let mut config_file_str = String::new();
    config_file.read_to_string(&mut config_file_str).unwrap();

    assert!(config_file_str.contains(&Endpoint::devnet().to_string()));
    Ok(())
}

#[tokio::test]
async fn init_with_testnet() -> Result<()> {
    let store_path = create_test_store_path();
    let endpoint = Endpoint::testnet();
    let temp_dir = init_cli_with_store_path(&store_path, &endpoint);

    // Check in the config file that the network is testnet
    let mut config_path = temp_dir.clone();
    config_path.push(MIDEN_DIR);
    config_path.push("miden-client.toml");
    let mut config_file = File::open(config_path).unwrap();
    let mut config_file_str = String::new();
    config_file.read_to_string(&mut config_file_str).unwrap();

    assert!(config_file_str.contains(&Endpoint::testnet().to_string()));
    Ok(())
}

#[tokio::test]
#[serial_test::file_serial]
async fn debug_mode_outputs_logs() -> Result<()> {
    // This test tries to execute a transaction with debug mode enabled and checks that the stack
    // state is printed. We need to use the CLI for this because the debug logs are always printed
    // to stdout and we can't capture them in a [`Client`] only test.
    // We use the [`Client`] to create a custom note that will print the stack state and consume it
    // using the CLI to check the stdout.
    const NOTE_FILENAME: &str = "test_note.mno";
    unsafe {
        env::set_var("MIDEN_DEBUG", "true");
    }

    // Create a Client and a custom note
    let (store_path, _, endpoint) = init_cli();
    let (mut client, authenticator) =
        create_rust_client_with_store_path(&store_path, endpoint).await?;
    let (account, ..) =
        insert_new_wallet(&mut client, AccountType::Private, &authenticator, RPO_FALCON_SCHEME_ID)
            .await?;

    // Create the custom note with a script that will print the stack state
    let note_script = "
            @note_script
            pub proc main
                debug.stack
                assert_eq
            end
            ";
    let note_script = client.code_builder().compile_note_script(note_script).unwrap();
    let inputs = NoteStorage::new(vec![]).unwrap();
    let serial_num = client.rng().draw_word();
    let note_metadata = PartialNoteMetadata::new(account.id(), NoteType::Private)
        .with_tag(NoteTag::with_account_target(account.id()));
    let note_assets = NoteAssets::new(vec![]).unwrap();
    let note_recipient = NoteRecipient::new(serial_num, note_script, inputs);
    let note = Note::new(note_assets, note_metadata, note_recipient);

    // Send transaction and wait for it to be committed
    client.sync_state().await?;
    let transaction_request =
        TransactionRequestBuilder::new().own_output_notes(vec![note.clone()]).build()?;
    execute_tx_and_sync(&mut client, account.id(), transaction_request).await?;

    // Export the note
    let note_file: NoteFile = NoteFile::NoteDetails {
        details: note.clone().into(),
        after_block_num: 0.into(),
        tag: Some(note.metadata().tag()),
    };

    // Import the note into the CLI
    let (_, temp_dir, _) = init_cli();

    // Serialize the note
    let note_path = temp_dir.join(NOTE_FILENAME);
    let mut file = File::create(note_path.clone()).unwrap();
    file.write_all(&note_file.to_bytes()).unwrap();

    // Import the note
    let mut import_cmd = cargo_bin_cmd!("miden-client");
    import_cmd.args(["import", note_path.to_str().unwrap()]);
    import_cmd.current_dir(&temp_dir).assert().success();

    sync_cli(&temp_dir);

    // Create wallet account
    let wallet_account_id = new_wallet_cli(&temp_dir, AccountType::Private);

    // Consume the note and check the output
    let mut consume_note_cmd = cargo_bin_cmd!("miden-client");
    let note_id = note.id().to_hex();
    let mut cli_args = vec!["consume-notes", "--account", &wallet_account_id, "--force"];
    cli_args.extend_from_slice(vec![note_id.as_str()].as_slice());
    consume_note_cmd.args(&cli_args);
    consume_note_cmd
        .current_dir(&temp_dir)
        .assert()
        .success()
        .stdout(contains("Stack state"));

    unsafe {
        env::remove_var("MIDEN_DEBUG");
    }

    Ok(())
}

// ADDRESSES TESTS
// ================================================================================================

#[tokio::test]
async fn list_addresses_add() -> Result<()> {
    let temp_dir = init_cli().1;

    // Create wallet account
    let basic_account_id = new_wallet_cli(&temp_dir, AccountType::Private);

    sync_cli(&temp_dir);

    let mut list_addresses_cmd = cargo_bin_cmd!("miden-client");
    list_addresses_cmd.args(["address", "list", &basic_account_id]);

    let output = list_addresses_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());
    let formatted_output = String::from_utf8(output.stdout).unwrap();
    assert!(formatted_output.contains(&basic_account_id));
    assert!(formatted_output.contains("Unspecified"));
    assert!(!formatted_output.contains("BasicWallet"));

    // Encode a BasicWallet address with tag length 10, then add it to the account.
    let encoded_address =
        encode_address_cli(&temp_dir, &basic_account_id, "basic-wallet", Some("10"));

    let mut add_address_cmd = cargo_bin_cmd!("miden-client");
    add_address_cmd.args(["address", "add", &basic_account_id, &encoded_address]);
    let output = add_address_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());

    // List of addresses for created account should now contain a BasicWallet address
    sync_cli(&temp_dir);
    let output = list_addresses_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());
    let formatted_output = String::from_utf8(output.stdout).unwrap();
    assert!(formatted_output.contains(&basic_account_id));
    assert_eq!(formatted_output.matches("Unspecified").count(), 1);
    assert_eq!(formatted_output.matches("BasicWallet").count(), 1);

    // Encode another BasicWallet address (different tag length → different address) and add it too.
    let encoded_address =
        encode_address_cli(&temp_dir, &basic_account_id, "basic-wallet", Some("5"));

    let mut add_address_cmd = cargo_bin_cmd!("miden-client");
    add_address_cmd.args(["address", "add", &basic_account_id, &encoded_address]);
    let output = add_address_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());

    // List of addresses for created account should now contain two BasicWallet addresses
    sync_cli(&temp_dir);
    let output = list_addresses_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());
    let formatted_output = String::from_utf8(output.stdout).unwrap();
    assert!(formatted_output.contains(&basic_account_id));
    assert_eq!(formatted_output.matches("Unspecified").count(), 1);
    assert_eq!(formatted_output.matches("BasicWallet").count(), 2);

    Ok(())
}

/// Verifies that `address add` rejects a bech32 address whose encoded account ID does not
/// match the `<ACCOUNT_ID>` argument.
#[tokio::test]
async fn address_add_rejects_mismatched_account() -> Result<()> {
    let temp_dir = init_cli().1;

    let account_a = new_wallet_cli(&temp_dir, AccountType::Private);
    let account_b = new_wallet_cli(&temp_dir, AccountType::Private);
    assert_ne!(account_a, account_b, "two new wallets should have distinct ids");

    sync_cli(&temp_dir);

    // Encode an address that points at account A.
    let encoded_for_a = encode_address_cli(&temp_dir, &account_a, "basic-wallet", None);

    // Trying to add it to account B must fail.
    let mut add_cmd = cargo_bin_cmd!("miden-client");
    add_cmd.args(["address", "add", &account_b, &encoded_for_a]);
    let output = add_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(!output.status.success(), "expected add to fail on account mismatch");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not match the provided account ID"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

#[tokio::test]
async fn address_add_rejects_mismatched_network() -> Result<()> {
    let temp_dir = init_cli().1;

    let account = new_wallet_cli(&temp_dir, AccountType::Private);
    sync_cli(&temp_dir);

    // Encode a valid address against the CLI's configured network, then re-encode it under a
    // different `NetworkId` so the HRP no longer matches.
    let encoded_local = encode_address_cli(&temp_dir, &account, "basic-wallet", None);
    let (cli_network_id, address) = Address::decode(&encoded_local)?;
    let other_network_id = if cli_network_id == NetworkId::Mainnet {
        NetworkId::Testnet
    } else {
        NetworkId::Mainnet
    };
    let encoded_other = address.encode(other_network_id);

    let mut add_cmd = cargo_bin_cmd!("miden-client");
    add_cmd.args(["address", "add", &account, &encoded_other]);
    let output = add_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(!output.status.success(), "expected add to fail on network mismatch");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not match configured network"),
        "unexpected stderr: {stderr}"
    );

    Ok(())
}

#[tokio::test]
async fn list_addresses_remove() -> Result<()> {
    let temp_dir = init_cli().1;

    // Create wallet account
    let basic_account_id = new_wallet_cli(&temp_dir, AccountType::Private);

    sync_cli(&temp_dir);

    // List of addresses for created account should contain an Unspecified address
    let mut list_addresses_cmd = cargo_bin_cmd!("miden-client");
    list_addresses_cmd.args(["address", "list", &basic_account_id]);
    let output = list_addresses_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());
    let formatted_output = String::from_utf8(output.stdout).unwrap();
    assert!(formatted_output.contains(&basic_account_id));
    assert_eq!(formatted_output.matches("Unspecified").count(), 1);

    // Remove the Unspecified wallet from the account
    let mut remove_address_cmd = cargo_bin_cmd!("miden-client");
    // Match any bech32 Miden address (HRP varies by network: mlcl, mdev, mtst, mm, etc.)
    let unspecified_wallet_address = regex::Regex::new(r"m[a-z]{1,4}1[0-9a-z]+")
        .unwrap()
        .find(&formatted_output)
        .unwrap()
        .as_str();
    remove_address_cmd.args(["address", "remove", &basic_account_id, unspecified_wallet_address]);
    let output = remove_address_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());

    // List of addresses for created account should now contain one BasicWallet address
    sync_cli(&temp_dir);
    let output = list_addresses_cmd.current_dir(temp_dir.clone()).output().unwrap();
    assert!(output.status.success());
    let formatted_output = String::from_utf8(output.stdout).unwrap();
    assert!(formatted_output.contains(&basic_account_id));
    assert_eq!(formatted_output.matches("Unspecified").count(), 0);

    Ok(())
}

#[tokio::test]
async fn new_wallet_with_deploy_flag() -> Result<()> {
    let (store_path, temp_dir, endpoint) = init_cli();

    sync_cli(&temp_dir);

    let mut create_wallet_cmd = cargo_bin_cmd!("miden-client");
    create_wallet_cmd.args(["new-wallet", "-t", "public", "--deploy"]);

    let output = create_wallet_cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        output.status.success(),
        "Failed to create and deploy wallet: {}",
        String::from_utf8(output.stderr).unwrap()
    );

    // Extract the account ID from the output
    let output_str = std::str::from_utf8(&output.stdout).unwrap();
    let account_id_str = output_str
        .split_whitespace()
        .skip_while(|&word| word != "-s")
        .nth(1)
        .expect("Failed to extract account ID from output");

    // Sync to ensure the transaction is committed
    sync_cli(&temp_dir);

    // Create a client and retrieve the account to verify the nonce
    let (client, _) = create_rust_client_with_store_path(&store_path, endpoint).await?;
    let account_id = AccountId::from_hex(account_id_str)?;
    let nonce = client.account_reader(account_id).nonce().await?;

    // Verify that the nonce is non-zero (account was deployed)
    // By convention, a nonce of 0 indicates an undeployed account
    assert!(
        nonce.as_canonical_u64() > 0,
        "Account nonce should be non-zero after deployment, but got: {nonce}"
    );

    Ok(())
}

// HELPERS
// ================================================================================================

/// Initializes a CLI with the network in the config file and returns the store path and the temp
/// directory where the CLI is running.
fn init_cli() -> (PathBuf, PathBuf, Endpoint) {
    // Try to read from env first or default to localhost.
    // Accepts "devnet", "testnet", "localhost", or a custom RPC endpoint string.
    let network: Network = std::env::var("TEST_MIDEN_NETWORK")
        .unwrap_or_else(|_| "localhost".to_string())
        .parse()
        .unwrap();
    let endpoint = Endpoint::try_from(network.to_rpc_endpoint().as_str()).unwrap();

    let store_path = create_test_store_path();
    let temp_dir = init_cli_with_store_path(&store_path, &endpoint);
    (store_path, temp_dir, endpoint)
}

/// Initializes a CLI with the given network and store path and returns the temp directory where
/// the CLI is running.
fn init_cli_with_store_path(store_path: &Path, endpoint: &Endpoint) -> PathBuf {
    let temp_dir = temp_dir().join(format!("cli-test-{}", rand::rng().random::<u64>()));
    std::fs::create_dir_all(&temp_dir).unwrap();

    // Init and create basic wallet on second client
    let mut init_cmd = cargo_bin_cmd!("miden-client");
    init_cmd.args([
        "init",
        "--local", // Use local mode to maintain test isolation
        "--network",
        endpoint.to_string().as_str(),
        "--store-path",
        store_path.to_str().unwrap(),
    ]);
    init_cmd.current_dir(&temp_dir).assert().success();

    temp_dir
}

/// Creates an isolated temporary directory and sets `MIDEN_CLIENT_HOME` to point to it.
/// This prevents tests from touching the real `~/.miden` directory.
/// Tests using this MUST use `#[serial_test::file_serial]`.
fn set_isolated_miden_home() -> PathBuf {
    let path = temp_dir().join(format!("miden-home-{}", rand::rng().random::<u64>()));
    std::fs::create_dir_all(&path).unwrap();
    // SAFETY: Tests using this are serialized via #[serial_test::file_serial]
    // These don't need to be executed in parallel as they aren't a bottleneck at all.
    unsafe {
        env::set_var("MIDEN_CLIENT_HOME", &path);
    }
    path
}

struct SyncResult {
    committed_notes: u64,
    committed_transactions: u64,
}

// Syncs CLI on directory. It'll try syncing until the command executes successfully. If it never
// executes successfully, eventually the test will time out (provided the nextest config has a
// timeout set). It returns the number of committed notes and transactions after the sync.
fn sync_cli(cli_path: &Path) -> SyncResult {
    loop {
        let mut sync_cmd = cargo_bin_cmd!("miden-client");
        sync_cmd.args(["sync"]);

        let output = sync_cmd.current_dir(cli_path).output().unwrap();

        if output.status.success() {
            let stdout = String::from_utf8(output.stdout).unwrap();

            let committed_notes = stdout
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Committed notes: ")
                        .and_then(|rest| rest.trim().parse::<u64>().ok())
                })
                .unwrap();

            let committed_transactions = stdout
                .lines()
                .find_map(|line| {
                    line.strip_prefix("Committed transactions: ")
                        .and_then(|rest| rest.trim().parse::<u64>().ok())
                })
                .unwrap();

            return SyncResult { committed_notes, committed_transactions };
        }
        std::thread::sleep(std::time::Duration::from_secs(3));
    }
}

/// Mints 100 units of the corresponding faucet using the cli and checks that the command runs
/// successfully given account using the CLI given by `cli_path`.
fn mint_cli(cli_path: &Path, target_account_id: &str, faucet_id: &str) -> String {
    let mut mint_cmd = cargo_bin_cmd!("miden-client");
    mint_cmd.env("MIDEN_DEBUG", "true");
    mint_cmd.args([
        "mint",
        "--target",
        target_account_id,
        "--asset",
        &format!("100::{faucet_id}"),
        "-n",
        "private",
        "--force",
    ]);

    let output = mint_cmd.current_dir(cli_path).output().unwrap();
    assert!(
        output.status.success(),
        "mint_cli failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .skip_while(|&word| word != "Output")
        .find(|word| word.starts_with("0x"))
        .unwrap()
        .to_string()
}

/// Shows note details using the cli and checks that the command runs
/// successfully given account using the CLI given by `cli_path`.
fn show_note_cli(cli_path: &Path, note_id: &str, should_fail: bool) {
    let mut show_note_cmd = cargo_bin_cmd!("miden-client");
    show_note_cmd.args(["notes", "--show", note_id]);

    if should_fail {
        show_note_cmd.current_dir(cli_path).assert().failure();
    } else {
        show_note_cmd.current_dir(cli_path).assert().success();
    }
}

/// Sends 25 units of the corresponding faucet and checks that the command runs successfully given
/// account using the CLI given by `cli_path`.
fn send_cli(cli_path: &Path, from_account_id: &str, to_account_id: &str, faucet_id: &str) {
    let mut send_cmd = cargo_bin_cmd!("miden-client");
    send_cmd.args([
        "send",
        "--sender",
        from_account_id,
        "--target",
        to_account_id,
        "--asset",
        &format!("25::{faucet_id}"),
        "-n",
        "private",
        "--force",
    ]);
    send_cmd.current_dir(cli_path).assert().success();
}

/// Syncs until a tracked note gets committed.
fn sync_until_committed_note(cli_path: &Path) {
    while sync_cli(cli_path).committed_notes == 0 {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Syncs until a tracked transaction gets committed.
fn sync_until_committed_transaction(cli_path: &Path) {
    while sync_cli(cli_path).committed_transactions == 0 {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Consumes a series of notes with a given account using the CLI given by `cli_path`.
fn consume_note_cli(cli_path: &Path, account_id: &str, note_ids: &[&str]) {
    let mut consume_note_cmd = cargo_bin_cmd!("miden-client");
    let mut cli_args = vec!["consume-notes", "--account", &account_id, "--force"];
    cli_args.extend_from_slice(note_ids);
    consume_note_cmd.args(&cli_args);
    consume_note_cmd.current_dir(cli_path).assert().success();
}

/// Creates a new faucet account using the CLI given by `cli_path`.
fn new_faucet_cli(cli_path: &Path, visibility: AccountType) -> String {
    const INIT_DATA_FILENAME: &str = "init_data.toml";
    let mut create_faucet_cmd = cargo_bin_cmd!("miden-client");

    // Create a TOML file with the InitStorageData
    let init_storage_data_toml = r#"
        [fungible-faucet-metadata]
        symbol = "BTC"
        decimals = 10
        max_supply = 10000000
        "#;
    let file_path = cli_path.join(INIT_DATA_FILENAME);
    fs::write(&file_path, init_storage_data_toml).unwrap();

    create_faucet_cmd.args([
        "new-account",
        "-t",
        visibility.to_string().as_str(),
        "-p",
        "basic-fungible-faucet",
        "-i",
        INIT_DATA_FILENAME,
    ]);
    create_faucet_cmd.current_dir(cli_path).assert().success();

    let output = create_faucet_cmd.current_dir(cli_path).output().unwrap();
    assert!(output.status.success());

    std::str::from_utf8(&output.stdout)
        .unwrap()
        .split_whitespace()
        .skip_while(|&word| word != "-s")
        .nth(1)
        .unwrap()
        .to_string()
}

/// Creates a new wallet account using the CLI given by `cli_path`.
fn new_wallet_cli(cli_path: &Path, visibility: AccountType) -> String {
    let mut create_wallet_cmd = cargo_bin_cmd!("miden-client");
    create_wallet_cmd.args(["new-wallet", "-t", visibility.to_string().as_str()]);

    let output = create_wallet_cmd.current_dir(cli_path).output().unwrap();
    assert!(
        output.status.success(),
        "Failed to create wallet {}",
        String::from_utf8(output.stderr)
            .map_or(". Also failed to access the Command's stderr".to_string(), |err_msg| format!(
                "with error: {err_msg}"
            ))
    );

    std::str::from_utf8(&output.stdout)
        .unwrap()
        .split_whitespace()
        .skip_while(|&word| word != "-s")
        .nth(1)
        .unwrap()
        .to_string()
}

/// Runs `miden-client address encode` and returns the printed bech32 address.
fn encode_address_cli(
    cli_path: &Path,
    account_id: &str,
    interface: &str,
    tag_len: Option<&str>,
) -> String {
    let mut encode_cmd = cargo_bin_cmd!("miden-client");
    let mut args = vec!["address", "encode", account_id, interface];
    if let Some(tag_len) = tag_len {
        args.push(tag_len);
    }
    encode_cmd.args(args);
    let output = encode_cmd.current_dir(cli_path).output().unwrap();
    assert!(
        output.status.success(),
        "address encode failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

pub type TestClient = Client<FilesystemKeyStore>;

/// Creates a new [`Client`] with a given store. Also returns the keystore associated with it.
async fn create_rust_client_with_store_path(
    store_path: &Path,
    endpoint: Endpoint,
) -> Result<(TestClient, FilesystemKeyStore)> {
    let store = {
        let sqlite_store = SqliteStore::new(PathBuf::from(store_path)).await?;
        std::sync::Arc::new(sqlite_store)
    };

    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();

    let rng = Box::new(RandomCoin::new(coin_seed.map(Felt::new_unchecked).into()));

    let keystore = FilesystemKeyStore::new(temp_dir())?;

    let client = ClientBuilder::new()
        .grpc_client(&endpoint, Some(10_000))
        .rng(rng)
        .store(store)
        .authenticator(Arc::new(keystore.clone()))
        .in_debug_mode(DebugMode::Enabled)
        .build()
        .await?;

    Ok((client, keystore))
}

/// Executes a command and asserts that it fails but does not panic.
fn assert_command_fails_but_does_not_panic(command: &mut Command) {
    let output_error = command.ok().unwrap_err();
    let exit_code = output_error.as_output().unwrap().status.code().unwrap();
    assert_ne!(exit_code, 0); // Command failed
    assert_ne!(exit_code, 101); // Command didn't panic
}

// COMMANDS TESTS
// ================================================================================================

#[test]
fn exec_parse() {
    let failure_script =
        fs::canonicalize("tests/files/test_cli_advice_inputs_expect_failure.masm").unwrap();
    let success_script =
        fs::canonicalize("tests/files/test_cli_advice_inputs_expect_success.masm").unwrap();
    let toml_path = fs::canonicalize("tests/files/test_cli_advice_inputs_input.toml").unwrap();

    let temp_dir = init_cli().1;

    // Create wallet account
    let basic_account_id = new_wallet_cli(&temp_dir, AccountType::Private);

    sync_cli(&temp_dir);
    let mut success_cmd = cargo_bin_cmd!("miden-client");
    success_cmd.args([
        "exec",
        "-s",
        success_script.to_str().unwrap(),
        "-a",
        &basic_account_id,
        "-i",
        toml_path.to_str().unwrap(),
    ]);

    success_cmd.current_dir(&temp_dir).assert().success();

    let mut failure_cmd = cargo_bin_cmd!("miden-client");
    failure_cmd.args([
        "exec",
        "-s",
        failure_script.to_str().unwrap(),
        "-a",
        &basic_account_id,
        "-i",
        toml_path.to_str().unwrap(),
    ]);

    failure_cmd.current_dir(&temp_dir).assert().failure();
}

// CALL COMMAND TESTS
// ================================================================================================

/// Tests that the `call` command fails when no arguments are provided.
#[test]
fn call_empty_command() {
    let temp_dir = init_cli().1;

    let mut cmd = cargo_bin_cmd!("miden-client");
    assert_command_fails_but_does_not_panic(cmd.args(["call"]).current_dir(&temp_dir));
}

/// Tests that the `call` command fails when the package file does not exist.
#[test]
fn call_nonexistent_package() {
    let temp_dir = init_cli().1;

    let basic_account_id = new_wallet_cli(&temp_dir, AccountType::Private);

    let mut cmd = cargo_bin_cmd!("miden-client");
    cmd.args([
        "call",
        &format!("{basic_account_id}:some_procedure"),
        "--package",
        "nonexistent/path/package.masp",
    ]);

    cmd.current_dir(&temp_dir).assert().failure();
}

/// Tests that the `call` command fails when the procedure name is not found in the package.
#[test]
fn call_nonexistent_procedure() {
    let temp_dir = init_cli().1;

    let basic_account_id = new_wallet_cli(&temp_dir, AccountType::Private);
    let package_path = temp_dir.join(MIDEN_DIR).join("packages/basic-wallet.masp");

    sync_cli(&temp_dir);

    let mut cmd = cargo_bin_cmd!("miden-client");
    cmd.args([
        "call",
        &format!("{basic_account_id}:nonexistent_procedure"),
        "--package",
        package_path.to_str().unwrap(),
    ]);

    cmd.current_dir(&temp_dir).assert().failure();
}

/// Helper: builds the `call-test` package (arithmetic + storage procedures) at runtime and
/// writes the serialized `.masp` to `out_path`.
fn call_test_exports(
    library: &miden_client::assembly::Library,
) -> Vec<miden_client::vm::PackageExport> {
    use miden_client::vm::{PackageExport, ProcedureExport, QualifiedProcedureName};
    use midenc_hir_type::{CallConv, FunctionType, Type};

    let signature_overrides: [(&str, FunctionType); 3] = [
        (
            "add",
            FunctionType::new(CallConv::ComponentModel, [Type::Felt, Type::Felt], [Type::Felt]),
        ),
        (
            "set_value",
            FunctionType::new(
                CallConv::ComponentModel,
                [Type::Felt, Type::Felt, Type::Felt, Type::Felt],
                [],
            ),
        ),
        ("read_advice", FunctionType::new(CallConv::ComponentModel, [], [Type::Felt])),
    ];

    let mut exports = Vec::new();
    for module_info in library.module_infos() {
        for (_, proc_info) in module_info.procedures() {
            let name = QualifiedProcedureName::new(module_info.path(), proc_info.name.clone());
            let override_sig = signature_overrides
                .iter()
                .find(|(n, _)| *n == proc_info.name.as_str())
                .map(|(_, sig)| sig.clone());
            exports.push(PackageExport::Procedure(ProcedureExport {
                path: name.into_inner(),
                digest: proc_info.digest,
                signature: override_sig.or_else(|| proc_info.signature.as_deref().cloned()),
                attributes: proc_info.attributes.clone(),
            }));
        }
    }
    exports
}

fn build_call_test_masp(out_path: &Path) {
    use miden_client::account::StorageSlotName;
    use miden_client::account::component::{
        AccountComponentMetadata,
        FeltSchema,
        StorageSchema,
        StorageSlotSchema,
        ValueSlotSchema,
        WordSchema,
    };
    use miden_client::assembly::{CodeBuilder, Library};
    use miden_client::vm::{Package, PackageManifest, Section, SectionId, TargetType};

    let call_test_code = r#"
        use miden::protocol::native_account
        use miden::core::word
        use miden::core::sys

        const STORED_VALUE = word("miden::testing::call_test::stored_value")

        pub proc add
            add
        end

        pub proc set_value
            push.STORED_VALUE[0..2]
            exec.native_account::set_item
            dropw
            exec.sys::truncate_stack
        end

        pub proc read_advice
            # Look up a fixed key in the advice map and return the sum of its two values.
            push.268435456.0.0.0
            adv.push_mapval
            dropw
            adv_push adv_push
            add
            exec.sys::truncate_stack
        end
    "#;

    let library: Library = CodeBuilder::default()
        .compile_component_code("miden::testing::call_test", call_test_code)
        .expect("failed to compile call-test component")
        .into();

    let slot_name =
        StorageSlotName::new("miden::testing::call_test::stored_value").expect("valid slot name");

    let word_schema = WordSchema::new_value([
        FeltSchema::new_void(),
        FeltSchema::new_void(),
        FeltSchema::new_void(),
        FeltSchema::new_void(),
    ]);

    let storage_schema = StorageSchema::new([(
        slot_name,
        StorageSlotSchema::Value(ValueSlotSchema::new(None, word_schema)),
    )])
    .expect("valid storage schema");

    let metadata = AccountComponentMetadata::new("call-test").with_storage_schema(storage_schema);

    let exports = call_test_exports(&library);
    let manifest = PackageManifest::new(exports).expect("manifest validation failed");
    let section = Section::new(SectionId::ACCOUNT_COMPONENT_METADATA, metadata.to_bytes());

    let package = Package {
        name: metadata.name().to_string().into(),
        version: metadata.version().clone(),
        description: Some(metadata.description().to_string()),
        mast: Arc::new(library),
        manifest,
        sections: vec![section],
        kind: TargetType::AccountComponent,
    };

    fs::write(out_path, package.to_bytes()).expect("failed to write call-test .masp");
}

/// Helper: creates an account with the `call-test.masp` package and returns (`temp_dir`,
/// `account_id`, `masp_path`).
fn setup_call_test_account() -> (PathBuf, String, PathBuf) {
    let temp_dir = init_cli().1;

    // Generate the call-test .masp directly in the temp dir
    let masp_dst = temp_dir.join("call_test.masp");
    build_call_test_masp(&masp_dst);

    // Init storage for the stored_value slot
    let init_toml = r#"
"miden::testing::call_test::stored_value" = "0x0000000000000000000000000000000000000000000000000000000000000000"
"#;
    let init_path = temp_dir.join("call_test_init.toml");
    fs::write(&init_path, init_toml).unwrap();

    // Create account with the custom package
    let mut create_cmd = cargo_bin_cmd!("miden-client");
    create_cmd.args([
        "new-account",
        "-t",
        "public",
        "-p",
        "auth/no-auth",
        "-p",
        masp_dst.to_str().unwrap(),
        "-i",
        init_path.to_str().unwrap(),
    ]);

    let output = create_cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        output.status.success(),
        "Failed to create account: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Parse account ID from output: "...account -s <ID>"
    let stdout = String::from_utf8_lossy(&output.stdout);
    let account_id = stdout
        .split_whitespace()
        .skip_while(|&w| w != "-s")
        .nth(1)
        .expect("Could not parse account ID from new-account output")
        .to_string();

    sync_cli(&temp_dir);

    (temp_dir, account_id, masp_dst)
}

/// Tests calling a procedure by name (add) with felt arguments.
#[test]
fn call_procedure_by_name() {
    let (temp_dir, account_id, masp_path) = setup_call_test_account();

    let mut cmd = cargo_bin_cmd!("miden-client");
    cmd.args([
        "call",
        &format!("{account_id}:add"),
        "3",
        "7",
        "--package",
        masp_path.to_str().unwrap(),
    ]);

    cmd.current_dir(&temp_dir).assert().success();
}

/// Tests that transaction execution produces a nonce change in the state delta.
#[test]
fn call_shows_nonce_delta() {
    let (temp_dir, account_id, masp_path) = setup_call_test_account();

    let mut cmd = cargo_bin_cmd!("miden-client");
    cmd.args([
        "call",
        &format!("{account_id}:add"),
        "1",
        "2",
        "--package",
        masp_path.to_str().unwrap(),
    ]);

    let output = cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        output.status.success(),
        "Call failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Nonce incremented by:"),
        "Expected nonce delta in output:\n{stdout}"
    );
}

/// Tests calling `set_value` and verifying storage delta is shown.
#[test]
fn call_set_value_shows_storage_delta() {
    let (temp_dir, account_id, masp_path) = setup_call_test_account();

    // set_value expects [VALUE (4 felts)] on the stack
    let mut cmd = cargo_bin_cmd!("miden-client");
    cmd.args([
        "call",
        &format!("{account_id}:set_value"),
        "42",
        "0",
        "0",
        "0",
        "--package",
        masp_path.to_str().unwrap(),
    ]);

    let output = cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        output.status.success(),
        "Call failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Storage Slot"), "Expected storage delta in output:\n{stdout}");
}

/// Tests that advice map entries supplied via `--inputs-path` reach the called procedure.
/// `read_advice` looks up a fixed key in the advice map and returns the sum of the two mapped
/// values (13 + 9 = 22).
#[test]
fn call_with_advice_inputs() {
    let (temp_dir, account_id, masp_path) = setup_call_test_account();

    let advice_path = fs::canonicalize("tests/files/test_cli_advice_inputs_input.toml").unwrap();

    let mut cmd = cargo_bin_cmd!("miden-client");
    cmd.args([
        "call",
        &format!("{account_id}:read_advice"),
        "--package",
        masp_path.to_str().unwrap(),
        "-i",
        advice_path.to_str().unwrap(),
    ]);

    let output = cmd.current_dir(&temp_dir).output().unwrap();
    assert!(
        output.status.success(),
        "Call with advice inputs failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Result: 22"),
        "Expected advice-derived result in output:\n{stdout}"
    );
}

/// Tests that calling a `add` with the wrong number of arguments fails
#[test]
fn call_rejects_wrong_arg_count() {
    let (temp_dir, account_id, masp_path) = setup_call_test_account();

    // Too few: 1 arg for a 2-arg procedure.
    let mut too_few = cargo_bin_cmd!("miden-client");
    too_few.args([
        "call",
        &format!("{account_id}:add"),
        "3",
        "--package",
        masp_path.to_str().unwrap(),
    ]);
    let out = too_few.current_dir(&temp_dir).output().unwrap();
    assert!(!out.status.success(), "Expected failure for too-few args");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("expects 2 argument") && stderr.contains("got 1"),
        "Unexpected stderr:\n{stderr}"
    );

    // Too many: 3 args for a 2-arg procedure.
    let mut too_many = cargo_bin_cmd!("miden-client");
    too_many.args([
        "call",
        &format!("{account_id}:add"),
        "3",
        "7",
        "11",
        "--package",
        masp_path.to_str().unwrap(),
    ]);
    let out = too_many.current_dir(&temp_dir).output().unwrap();
    assert!(!out.status.success(), "Expected failure for too-many args");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("expects 2 argument") && stderr.contains("got 3"),
        "Unexpected stderr:\n{stderr}"
    );
}

// AUTH COMPONENT TESTS
// ================================================================================================

/// Tests creating an account with the no-auth component.
#[test]
fn create_account_with_no_auth() {
    let temp_dir = init_cli().1;

    let mut create_account_cmd = cargo_bin_cmd!("miden-client");
    create_account_cmd.args([
        "new-account",
        "-t",
        "private",
        "-p",
        "basic-wallet",
        "-p",
        "auth/no-auth",
    ]);

    create_account_cmd.current_dir(&temp_dir).assert().success();
}

/// Tests creating an account with the multisig-auth component.
#[test]
fn create_account_with_multisig_auth() {
    let temp_dir = init_cli().1;

    // Create init storage data file for multisig
    // threshold_config is a value slot with [threshold, num_approvers, 0, 0]
    // approver_public_keys and procedure_thresholds are map slots
    let init_storage_data_toml = r#"
        "miden::standards::auth::multisig::threshold_config.threshold" = "2"
        "miden::standards::auth::multisig::threshold_config.num_approvers" = "3"

        "miden::standards::auth::multisig::approver_public_keys" = [
            { key = ["0", "0", "0", "0"], value = "0x0000000000000000000000000000000000000000000000000000000000000001" },
            { key = ["1", "0", "0", "0"], value = "0x0000000000000000000000000000000000000000000000000000000000000002" },
            { key = ["2", "0", "0", "0"], value = "0x0000000000000000000000000000000000000000000000000000000000000003" }
        ]

        "miden::standards::auth::multisig::approver_schemes" = [
            { key = ["0", "0", "0", "0"], value = ["2", "0", "0", "0"] },
            { key = ["1", "0", "0", "0"], value = ["2", "0", "0", "0"] },
            { key = ["2", "0", "0", "0"], value = ["2", "0", "0", "0"] }
        ]

        "miden::standards::auth::multisig::procedure_thresholds" = [
            { key = "0xd2d1b6229d7cfb9f2ada31c5cb61453cf464f91828e124437c708eec55b9cd07", value = "1" }
        ]
        "#;
    let file_path = temp_dir.join("multisig_init_data.toml");
    fs::write(&file_path, init_storage_data_toml).unwrap();

    let mut create_account_cmd = cargo_bin_cmd!("miden-client");
    create_account_cmd.args([
        "new-account",
        "-t",
        "private",
        "-p",
        "basic-wallet",
        "-p",
        "auth/multisig-auth",
        "-i",
        "multisig_init_data.toml",
    ]);

    create_account_cmd.current_dir(&temp_dir).assert().success();
}

/// Tests creating an account with the acl-auth component.
#[test]
fn create_account_with_acl_auth() {
    let temp_dir = init_cli().1;

    // Create init storage data file for acl-auth with a test public key
    let init_storage_data_toml = r#"
        "miden::standards::auth::singlesig_acl::pub_key" = "0x0000000000000000000000000000000000000000000000000000000000000001"
        "miden::standards::auth::singlesig_acl::scheme" = "Falcon512Poseidon2"
        "miden::standards::auth::singlesig_acl::config.num_trigger_procs" = "1"
        "miden::standards::auth::singlesig_acl::config.allow_unauthorized_output_notes" = "0"
        "miden::standards::auth::singlesig_acl::config.allow_unauthorized_input_notes" = "0"

        "miden::standards::auth::singlesig_acl::trigger_procedure_roots" = [
            { key = ["0", "0", "0", "0"], value = "0xd2d1b6229d7cfb9f2ada31c5cb61453cf464f91828e124437c708eec55b9cd07" }
        ]
        "#;
    let file_path = temp_dir.join("acl_init_data.toml");
    fs::write(&file_path, init_storage_data_toml).unwrap();

    let mut create_account_cmd = cargo_bin_cmd!("miden-client");
    create_account_cmd.args([
        "new-account",
        "-t",
        "private",
        "-p",
        "basic-wallet",
        "-p",
        "auth/acl-auth",
        "-i",
        "acl_init_data.toml",
    ]);

    create_account_cmd.current_dir(&temp_dir).assert().success();
}

// Tests creating an account with the acl-auth component.
#[test]
fn create_account_with_ecdsa_auth() {
    let temp_dir = init_cli().1;

    // Create init storage data file for ecdsa-auth with a test public key and scheme
    let init_storage_data_toml = r#"
        "miden::standards::auth::singlesig::pub_key" = "0x0000000000000000000000000000000000000000000000000000000000000001"
        "miden::standards::auth::singlesig::scheme" = "EcdsaK256Keccak"
        "#;
    let file_path = temp_dir.join("ecdsa_init_data.toml");
    fs::write(&file_path, init_storage_data_toml).unwrap();

    let mut create_account_cmd = cargo_bin_cmd!("miden-client");
    create_account_cmd.args([
        "new-account",
        "-t",
        "private",
        "-p",
        "basic-wallet",
        "-p",
        "auth/ecdsa-auth",
        "-i",
        "ecdsa_init_data.toml",
    ]);

    create_account_cmd.current_dir(&temp_dir).assert().success();
}

// CLICLIENT::NEW TESTS
// ================================================================================================
/// Tests that `CliClient::new()` successfully creates a client with the same
/// configuration as the CLI tool when a local config exists.
#[tokio::test]
#[serial_test::file_serial]
async fn test_new_with_local_config() -> Result<()> {
    // Initialize a local CLI configuration
    let (store_path, temp_dir, _endpoint) = init_cli();

    // Use isolated global miden directory to ensure no global config interferes
    let _miden_home = set_isolated_miden_home();

    // Change to the temp directory where local .miden config exists
    let original_dir = env::current_dir().unwrap();
    env::set_current_dir(&temp_dir)?;

    // Create a client using new - should pick up local config
    let client_result = miden_client_cli::CliClient::new(DebugMode::Disabled).await;

    // Restore original directory
    env::set_current_dir(original_dir)?;

    // Assert the client was created successfully
    assert!(
        client_result.is_ok(),
        "Failed to create client from local config: {:?}",
        client_result.err()
    );

    // Verify that the local config was actually used by checking which store file was created.
    // The local store should exist, indicating the local config was used.
    assert!(
        store_path.exists(),
        "Local store file should exist at {store_path:?}, indicating local config was used"
    );

    Ok(())
}

/// Tests that `CliClient::new()` silently initializes with default config
/// when no configuration exists.
#[tokio::test]
#[serial_test::file_serial]
async fn test_new_silent_init() -> Result<()> {
    // Create a temporary directory with no .miden configuration
    let temp_dir = temp_dir().join(format!("cli-test-silent-init-{}", rand::rng().random::<u64>()));
    std::fs::create_dir_all(&temp_dir)?;

    // Use isolated global miden directory
    let miden_home = set_isolated_miden_home();

    // Verify no config exists before we start
    let global_config_path = miden_home.join("miden-client.toml");
    assert!(!global_config_path.exists(), "Global config should not exist before test");

    // Change to the temp directory
    let original_dir = env::current_dir().unwrap();
    env::set_current_dir(&temp_dir)?;

    // Create a client - should succeed via silent initialization
    let client_result = miden_client_cli::CliClient::new(DebugMode::Disabled).await;

    // Restore original directory
    env::set_current_dir(original_dir)?;

    // Assert the client was created successfully
    assert!(
        client_result.is_ok(),
        "Expected client to be created via silent initialization, but got error: {:?}",
        client_result.err()
    );

    // Verify that a global config was created by the silent initialization
    assert!(
        global_config_path.exists(),
        "Expected global config to be created at {global_config_path:?} by silent initialization"
    );

    Ok(())
}

/// Tests that `CliConfig::load()` prioritizes local config over global config.
#[tokio::test]
#[serial_test::file_serial]
async fn test_load_local_priority() -> Result<()> {
    // Use isolated global miden directory
    let _miden_home = set_isolated_miden_home();

    // Create a global config with testnet endpoint
    let global_store_path = create_test_store_path();
    let global_endpoint = Endpoint::testnet();

    let temp_dir_for_global =
        temp_dir().join(format!("cli-test-global-init-{}", rand::rng().random::<u64>()));
    std::fs::create_dir_all(&temp_dir_for_global)?;

    let mut init_global_cmd = cargo_bin_cmd!("miden-client");
    init_global_cmd.args([
        "init",
        "--network",
        global_endpoint.to_string().as_str(),
        "--store-path",
        global_store_path.to_str().unwrap(),
    ]);
    init_global_cmd.current_dir(&temp_dir_for_global).assert().success();

    // Create a local config with localhost endpoint
    let local_store_path = create_test_store_path();
    let local_endpoint = Endpoint::localhost();
    let local_temp_dir = init_cli_with_store_path(&local_store_path, &local_endpoint);

    // Load config from the specific local directory (no need to change working directory!)
    let local_miden_dir = local_temp_dir.join(MIDEN_DIR);
    let config = miden_client_cli::CliConfig::from_dir(&local_miden_dir)?;

    // Create client with local config
    let client = miden_client_cli::CliClient::from_config(config, DebugMode::Disabled).await;

    // Assert client was created with local config
    assert!(client.is_ok(), "Failed to create client with local config: {:?}", client.err());

    // Verify that the local config was actually used by checking which store file was created

    // The local store should exist
    assert!(
        local_store_path.exists(),
        "Local store file should exist at {local_store_path:?}, indicating local config was used"
    );

    // The global store should NOT exist
    assert!(
        !global_store_path.exists(),
        "Global store file should NOT exist at {global_store_path:?}, as global config should not have been used"
    );

    Ok(())
}

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use miden_agglayer::create_bridge_account;
use miden_client::Deserializable;
use miden_client::account::{AccountFile, AccountId, AccountType};
use miden_client::auth::RPO_FALCON_SCHEME_ID;
use miden_client::crypto::FeltRng;
use miden_client::keystore::Keystore;
use miden_client::testing::common::{
    FilesystemKeyStore,
    TestClient,
    insert_new_wallet,
    wait_for_node,
    wait_for_tx,
};
use miden_client::transaction::TransactionRequestBuilder;

use crate::tests::config::ClientConfig;

pub mod agglayer_bridge_in_out;
mod agglayer_test_utils;
pub mod ger;
pub mod note_reader;

/// `AggLayer` network ID assigned to the Miden chain (the protocol's `MIDEN_NETWORK_ID` MASM
/// constant). Claim validation compares the leaf's `destination_network` to this value, so it
/// must match the `MIDEN_NETWORK_ID` constant in the foundry vectors
/// (`foundry-vectors/test/ClaimAssetTestVectorsLocalTx.t.sol`).
pub const MIDEN_AGGLAYER_NETWORK_ID: u32 = 77;

// AGGLAYER CONFIG
// ================================================================================================

/// Configuration for agglayer tests when running against a node with pre-deployed
/// agglayer accounts (e.g. complete genesis or devnet).
///
/// Loaded from `.mac` files in the directory specified by `AGGLAYER_ACCOUNTS_DIR` env var.
/// Account IDs and keys are read from files, but the actual account state is fetched
/// from the network to ensure it's up-to-date (idempotent across repeated runs).
pub struct AgglayerConfig {
    pub bridge_admin: AccountFile,
    pub ger_manager: AccountFile,
    pub bridge: AccountFile,
    pub faucet: AccountFile,
}

impl AgglayerConfig {
    /// File names matching the gen-genesis output (see the test-node-genesis crate).
    const BRIDGE_ADMIN_FILE: &str = "bridge_admin.mac";
    const GER_MANAGER_FILE: &str = "ger_manager.mac";
    const BRIDGE_FILE: &str = "bridge.mac";
    const FAUCET_FILE: &str = "agglayer_faucet.mac";

    /// Tries to load agglayer config from the `AGGLAYER_ACCOUNTS_DIR` env var.
    /// Returns `None` if the env var is not set.
    pub fn from_env() -> Result<Option<Self>> {
        match std::env::var("AGGLAYER_ACCOUNTS_DIR") {
            Ok(dir) => {
                let dir = PathBuf::from(dir);
                let bridge_admin = Self::load_account_file(&dir, Self::BRIDGE_ADMIN_FILE)?;
                let ger_manager = Self::load_account_file(&dir, Self::GER_MANAGER_FILE)?;
                let bridge = Self::load_account_file(&dir, Self::BRIDGE_FILE)?;
                let faucet = Self::load_account_file(&dir, Self::FAUCET_FILE)?;
                Ok(Some(Self {
                    bridge_admin,
                    ger_manager,
                    bridge,
                    faucet,
                }))
            },
            Err(_) => Ok(None),
        }
    }

    pub fn bridge_admin_id(&self) -> AccountId {
        self.bridge_admin.account.id()
    }

    pub fn ger_manager_id(&self) -> AccountId {
        self.ger_manager.account.id()
    }

    pub fn bridge_id(&self) -> AccountId {
        self.bridge.account.id()
    }

    pub fn faucet_id(&self) -> AccountId {
        self.faucet.account.id()
    }

    /// Imports a single account (by ID) into the given client and keystore.
    /// Fetches the latest state from the network. Adds any matching secret keys.
    pub async fn import_account(
        &self,
        account_id: AccountId,
        client: &mut TestClient,
        keystore: &FilesystemKeyStore,
    ) -> Result<()> {
        let account_file = [&self.bridge_admin, &self.ger_manager, &self.bridge, &self.faucet]
            .into_iter()
            .find(|f| f.account.id() == account_id)
            .with_context(|| format!("account {account_id} not found in agglayer config"))?;

        client
            .import_account_by_id(account_id)
            .await
            .with_context(|| format!("failed to import account {account_id} from network"))?;

        for secret_key in &account_file.auth_secret_keys {
            keystore.add_key(secret_key, account_id).await.with_context(|| {
                format!("failed to add key for account {account_id} to keystore")
            })?;
        }
        Ok(())
    }

    fn load_account_file(dir: &Path, filename: &str) -> Result<AccountFile> {
        let path = dir.join(filename);
        let bytes =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        AccountFile::read_from_bytes(&bytes)
            .map_err(|e| anyhow::anyhow!("failed to deserialize {}: {}", path.display(), e))
    }
}

// SHARED TEST SETUP
// ================================================================================================

/// A client + keystore pair for a single test entity.
pub struct ClientPair {
    pub client: TestClient,
    pub keystore: FilesystemKeyStore,
}

/// Account IDs produced by the core setup: `(bridge_admin_id, ger_manager_id, bridge_id)`.
pub type CoreAccountIds = (AccountId, AccountId, AccountId);

/// Creates three clients sharing the same RPC endpoint, for bridge admin, GER manager, and user.
pub async fn create_agglayer_clients(
    client_config: &ClientConfig,
) -> Result<(ClientPair, ClientPair, ClientPair)> {
    let (mut client, keystore) = client_config.clone().into_client().await?;
    wait_for_node(&mut client).await;
    client.sync_state().await?;
    println!("[setup] Bridge admin client initialized");
    let bridge_admin = ClientPair { client, keystore };

    let (client, keystore) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    println!("[setup] GER manager client initialized");
    let ger_manager = ClientPair { client, keystore };

    let (client, keystore) = ClientConfig::default()
        .with_rpc_endpoint(client_config.rpc_endpoint())
        .into_client()
        .await?;
    println!("[setup] User client initialized");
    let user = ClientPair { client, keystore };

    Ok((bridge_admin, ger_manager, user))
}

/// Sets up the core agglayer accounts (bridge admin, GER manager, bridge) across 3 clients.
///
/// Two modes:
/// - **Genesis** (`config` is `Some`, i.e. `AGGLAYER_ACCOUNTS_DIR` is set): the bridge admin, GER
///   manager and bridge are pre-deployed at genesis and imported from `.mac` files.
/// - **Runtime** (`config` is `None`): the bridge admin and GER manager wallets are created on
///   their clients, and the bridge (`AuthNetworkAccount`) is created and deployed within the test;
///   the faucet is registered against it later via the `CONFIG_AGG_BRIDGE` note.
pub async fn setup_core_accounts(
    config: Option<&AgglayerConfig>,
    bridge_admin: &mut ClientPair,
    ger_manager: &mut ClientPair,
    user: &mut ClientPair,
) -> Result<CoreAccountIds> {
    if let Some(config) = config {
        println!("[setup] Loading core accounts from genesis");
        println!("[setup]   bridge admin:  {}", config.bridge_admin_id());
        println!("[setup]   GER manager:   {}", config.ger_manager_id());
        println!("[setup]   bridge:        {}", config.bridge_id());

        config
            .import_account(
                config.bridge_admin_id(),
                &mut bridge_admin.client,
                &bridge_admin.keystore,
            )
            .await?;
        config
            .import_account(config.ger_manager_id(), &mut ger_manager.client, &ger_manager.keystore)
            .await?;

        for pair in [&mut *bridge_admin, &mut *ger_manager, &mut *user] {
            config
                .import_account(config.bridge_id(), &mut pair.client, &pair.keystore)
                .await?;
        }

        return Ok((config.bridge_admin_id(), config.ger_manager_id(), config.bridge_id()));
    }

    println!("[setup] Creating core accounts at runtime");

    // Bridge admin and GER manager are ordinary wallets, created on their own clients.
    let (bridge_admin_account, ..) = insert_new_wallet(
        &mut bridge_admin.client,
        AccountType::Public,
        &bridge_admin.keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;
    let (ger_manager_account, ..) = insert_new_wallet(
        &mut ger_manager.client,
        AccountType::Public,
        &ger_manager.keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await?;

    // The bridge is an `AuthNetworkAccount`. Create it (unconfigured) and distribute it to all
    // three clients so each can build transactions that reference it.
    let bridge_seed = bridge_admin.client.rng().draw_word();
    let bridge_account = create_bridge_account(
        bridge_seed,
        bridge_admin_account.id(),
        ger_manager_account.id(),
        MIDEN_AGGLAYER_NETWORK_ID,
    );
    println!("[setup]   bridge admin:  {}", bridge_admin_account.id());
    println!("[setup]   GER manager:   {}", ger_manager_account.id());
    println!("[setup]   bridge:        {}", bridge_account.id());

    for pair in [&mut *bridge_admin, &mut *ger_manager, &mut *user] {
        pair.client.add_account(&bridge_account, false).await?;
    }

    // Deploy the bridge account.
    let deploy_tx = TransactionRequestBuilder::new().build()?;
    let tx_id = bridge_admin
        .client
        .submit_new_transaction(bridge_account.id(), deploy_tx)
        .await?;
    wait_for_tx(&mut bridge_admin.client, tx_id).await?;
    println!("[setup] Bridge account deployed on-chain");

    Ok((bridge_admin_account.id(), ger_manager_account.id(), bridge_account.id()))
}

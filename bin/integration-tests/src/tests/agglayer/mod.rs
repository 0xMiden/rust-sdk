use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use miden_client::Deserializable;
use miden_client::account::{AccountFile, AccountId};
use miden_client::keystore::Keystore;
use miden_client::testing::common::{FilesystemKeyStore, TestClient, wait_for_node};
use miden_client_test_harness::ClientConfig;
use miden_client_test_harness::fee_funding::AccountLock;

pub mod agglayer_bridge_in_out;
mod agglayer_test_utils;
pub mod ger;
pub mod note_reader;

/// Env var naming the directory the pre-deployed agglayer account files are read from.
const ACCOUNTS_DIR_ENV: &str = "AGGLAYER_ACCOUNTS_DIR";

// AGGLAYER CONFIG
// ================================================================================================

/// The pre-deployed agglayer accounts the tests transact with.
///
/// Loaded from `.mac` files in the directory named by [`ACCOUNTS_DIR_ENV`]. Account IDs and keys
/// are read from the files, but the account state is fetched from the network so repeated runs
/// against the same chain stay idempotent.
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

    /// Loads the agglayer accounts from the directory named by [`ACCOUNTS_DIR_ENV`].
    pub fn from_env() -> Result<Self> {
        let dir = std::env::var(ACCOUNTS_DIR_ENV).map(PathBuf::from).with_context(|| {
            format!(
                "the agglayer accounts cannot be created by a test, so {ACCOUNTS_DIR_ENV} has to \
                 name the directory holding the `.mac` files of the ones deployed on this chain"
            )
        })?;

        Ok(Self {
            bridge_admin: Self::load_account_file(&dir, Self::BRIDGE_ADMIN_FILE)?,
            ger_manager: Self::load_account_file(&dir, Self::GER_MANAGER_FILE)?,
            bridge: Self::load_account_file(&dir, Self::BRIDGE_FILE)?,
            faucet: Self::load_account_file(&dir, Self::FAUCET_FILE)?,
        })
    }

    /// Claims the agglayer accounts for the calling test, waiting for whichever test holds them.
    ///
    /// Every agglayer test drives the same four accounts, so two at once would build transactions
    /// from the same nonce. One lock keyed on the bridge covers all four, where a lock per account
    /// could deadlock. The guard must stay alive for the whole test.
    pub fn claim(&self) -> Result<AccountLock> {
        AccountLock::acquire(self.bridge_id())
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

    let (client, keystore) = client_config.clone().with_fresh_store().into_client().await?;
    println!("[setup] GER manager client initialized");
    let ger_manager = ClientPair { client, keystore };

    let (client, keystore) = client_config.clone().with_fresh_store().into_client().await?;
    println!("[setup] User client initialized");
    let user = ClientPair { client, keystore };

    Ok((bridge_admin, ger_manager, user))
}

/// Imports the core agglayer accounts (bridge admin, GER manager, bridge) into the three clients.
///
/// The bridge goes into all three so each can build transactions that reference it. The bridge
/// admin and the GER manager go only into the client that signs for them.
pub async fn setup_core_accounts(
    config: &AgglayerConfig,
    bridge_admin: &mut ClientPair,
    ger_manager: &mut ClientPair,
    user: &mut ClientPair,
) -> Result<CoreAccountIds> {
    println!("[setup] Loading core accounts");
    println!("[setup]   bridge admin:  {}", config.bridge_admin_id());
    println!("[setup]   GER manager:   {}", config.ger_manager_id());
    println!("[setup]   bridge:        {}", config.bridge_id());

    config
        .import_account(config.bridge_admin_id(), &mut bridge_admin.client, &bridge_admin.keystore)
        .await?;
    config
        .import_account(config.ger_manager_id(), &mut ger_manager.client, &ger_manager.keystore)
        .await?;

    for pair in [&mut *bridge_admin, &mut *ger_manager, &mut *user] {
        config
            .import_account(config.bridge_id(), &mut pair.client, &pair.keystore)
            .await?;
    }

    Ok((config.bridge_admin_id(), config.ger_manager_id(), config.bridge_id()))
}

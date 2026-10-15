use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use miden_client::account::{AccountFile, AccountId};
use miden_client::testing::common::TestClient;
use tracing::info;

use crate::ClientConfig;

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

    pub fn bridge_admin_id(&self) -> AccountId {
        self.bridge_admin.account().id()
    }

    pub fn ger_manager_id(&self) -> AccountId {
        self.ger_manager.account().id()
    }

    pub fn bridge_id(&self) -> AccountId {
        self.bridge.account().id()
    }

    pub fn faucet_id(&self) -> AccountId {
        self.faucet.account().id()
    }

    fn load_account_file(dir: &Path, filename: &str) -> Result<AccountFile> {
        let path = dir.join(filename);
        let bytes =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        AccountFile::try_from_bytes(&bytes)
            .with_context(|| format!("failed to deserialize {}", path.display()))
    }
}

// SHARED TEST SETUP
// ================================================================================================

/// The pre-deployed agglayer accounts and the three clients that transact with them.
pub struct AgglayerScenario {
    pub config: AgglayerConfig,
    /// Signs for the bridge admin.
    pub bridge_admin: TestClient,
    /// Signs for the GER manager.
    pub ger_manager: TestClient,
    /// Holds the end-user accounts.
    pub user: TestClient,
}

impl AgglayerScenario {
    /// Loads the accounts named by [`ACCOUNTS_DIR_ENV`], creates the three clients and imports the
    /// core accounts. The bridge admin and the GER manager go into the client that signs for them.
    /// The bridge goes into all three so each can build transactions that reference it.
    pub async fn start(client_config: &ClientConfig) -> Result<Self> {
        let config = AgglayerConfig::from_env()?;
        let mut bridge_admin = client_config.clone().into_client().await?;
        let mut ger_manager = client_config.clone().into_client().await?;
        let mut user = client_config.clone().into_client().await?;

        info!(
            bridge_admin_id = %config.bridge_admin_id(),
            ger_manager_id = %config.ger_manager_id(),
            bridge_id = %config.bridge_id(),
            "Loading core accounts"
        );

        bridge_admin.import_account_file(&config.bridge_admin).await?;
        ger_manager.import_account_file(&config.ger_manager).await?;
        for client in [&mut bridge_admin, &mut ger_manager, &mut user] {
            client.import_account_file(&config.bridge).await?;
        }

        Ok(Self { config, bridge_admin, ger_manager, user })
    }
}

use std::env::temp_dir;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use miden_client::builder::ClientBuilder;
use miden_client::crypto::RandomCoin;
use miden_client::grpc_support::{DEVNET_PROVER_ENDPOINT, TESTNET_PROVER_ENDPOINT};
use miden_client::note_transport::grpc::GrpcNoteTransportClient;
use miden_client::note_transport::{
    NOTE_TRANSPORT_DEVNET_ENDPOINT,
    NOTE_TRANSPORT_TESTNET_ENDPOINT,
};
use miden_client::rpc::{Endpoint, GrpcClient, VerifyingRpcClient};
use miden_client::testing::common::{FilesystemKeyStore, TestClient, create_test_store_path};
use miden_client::testing::fee::FeeFunder;
use miden_client::{Felt, RemoteTransactionProver};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use rand::RngExt;
use uuid::Uuid;

use crate::fee_funding;

const NETWORK_DEVNET: &str = "devnet";
const NETWORK_TESTNET: &str = "testnet";
const NETWORK_LOCALHOST: &str = "localhost";

/// Identifies the note transport service to connect to.
#[derive(Clone, Debug)]
pub enum NoteTransportEndpoint {
    Devnet,
    Testnet,
    Custom(String),
}

impl NoteTransportEndpoint {
    /// Returns the gRPC URL for this endpoint.
    pub fn to_url(&self) -> String {
        match self {
            Self::Devnet => NOTE_TRANSPORT_DEVNET_ENDPOINT.to_string(),
            Self::Testnet => NOTE_TRANSPORT_TESTNET_ENDPOINT.to_string(),
            Self::Custom(url) => url.clone(),
        }
    }
}

impl FromStr for NoteTransportEndpoint {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "devnet" => Self::Devnet,
            "testnet" => Self::Testnet,
            _ => Self::Custom(s.to_string()),
        })
    }
}

impl fmt::Display for NoteTransportEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Devnet => write!(f, "devnet ({NOTE_TRANSPORT_DEVNET_ENDPOINT})"),
            Self::Testnet => write!(f, "testnet ({NOTE_TRANSPORT_TESTNET_ENDPOINT})"),
            Self::Custom(url) => write!(f, "{url}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub rpc_endpoint: Endpoint,
    pub rpc_timeout_ms: u64,
    /// Optional remote prover endpoint. If set, the client will use a remote prover instead of the
    /// default local prover.
    pub prover_endpoint: Option<String>,
    /// Optional note transport endpoint. If set, the client will connect to a note transport
    /// service.
    pub note_transport_endpoint: Option<NoteTransportEndpoint>,
    /// Funder the account-creating test helpers draw the native fee asset from. Shared by every
    /// client built from this config and its clones, so consecutive payments from one wallet chain
    /// off each other's nonce.
    pub fee_funder: Option<Arc<dyn FeeFunder>>,
}

impl ClientConfig {
    pub fn new(rpc_endpoint: Endpoint, rpc_timeout_ms: u64) -> Self {
        Self {
            rpc_endpoint,
            rpc_timeout_ms,
            prover_endpoint: None,
            note_transport_endpoint: None,
            fee_funder: None,
        }
    }

    #[allow(clippy::return_self_not_must_use)]
    pub fn with_prover_endpoint(mut self, prover_endpoint: Option<String>) -> Self {
        self.prover_endpoint = prover_endpoint;
        self
    }

    #[allow(clippy::return_self_not_must_use)]
    pub fn with_note_transport_endpoint(
        mut self,
        note_transport_endpoint: Option<NoteTransportEndpoint>,
    ) -> Self {
        self.note_transport_endpoint = note_transport_endpoint;
        self
    }

    /// Sets the funder the account-creating test helpers draw the native fee asset from.
    #[allow(clippy::return_self_not_must_use)]
    pub fn with_fee_funder(mut self, fee_funder: Option<Arc<dyn FeeFunder>>) -> Self {
        self.fee_funder = fee_funder;
        self
    }

    /// Loads the pre-funded wallets at `funders`, one `.mac` account file or a directory of them,
    /// as the fee funder. `None` leaves the config without one, which is all a fee-free chain
    /// needs.
    pub fn with_funders(self, funders: Option<&Path>) -> Result<Self> {
        let fee_funder = fee_funding::load(&self, funders)?;
        Ok(self.with_fee_funder(fee_funder))
    }

    /// Creates a `TestClient` builder and keystore.
    ///
    /// The store is a `SQLite` database at a temporary location, and the keystore a temporary
    /// directory, both created here rather than held on the config, so every client this is called
    /// on gets its own.
    pub fn into_client_builder(
        self,
    ) -> Result<(ClientBuilder<FilesystemKeyStore>, FilesystemKeyStore)> {
        let store_config = create_test_store_path();
        let auth_path = create_test_auth_path();

        let mut rng = rand::rng();
        let coin_seed: [u64; 4] = rng.random();

        let rng = RandomCoin::new(coin_seed.map(Felt::new_unchecked).into());

        let keystore = FilesystemKeyStore::new(auth_path.clone()).with_context(|| {
            format!("failed to create keystore at path: {}", auth_path.to_string_lossy())
        })?;

        let rpc_client = Arc::new(VerifyingRpcClient::new(GrpcClient::new(
            &self.rpc_endpoint,
            self.rpc_timeout_ms,
        )));

        let mut builder = ClientBuilder::new()
            .rpc(rpc_client)
            .rng(Box::new(rng))
            .sqlite_store(store_config)
            .authenticator(Arc::new(keystore.clone()))
            .tx_discard_delta(None);

        if let Some(prover_url) = &self.prover_endpoint {
            builder = builder.prover(Arc::new(RemoteTransactionProver::new(prover_url)));
        }

        if let Some(transport) = &self.note_transport_endpoint {
            let transport_url = transport.to_url();
            let transport_timeout = std::env::var("MIDEN_TEST_TIMEOUT")
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(10_000);
            let nt_client =
                Arc::new(GrpcNoteTransportClient::new(transport_url.clone(), transport_timeout));
            builder = builder.note_transport(nt_client);
        }

        Ok((builder, keystore))
    }

    /// Creates a `TestClient` without syncing it, for tests that have to wait for the node first.
    ///
    /// The client gets its own store and keystore.
    pub async fn into_unsynced_client(self) -> Result<(TestClient, FilesystemKeyStore)> {
        let fee_funder = self.fee_funder.clone();
        let (builder, keystore) = self.into_client_builder()?;

        let client = builder.build().await.with_context(|| "failed to build test client")?;

        Ok((TestClient::from(client).with_fee_funder(fee_funder), keystore))
    }

    /// Creates a `TestClient`.
    ///
    /// The client gets its own store and keystore, and is synced to the current state before being
    /// returned.
    pub async fn into_client(self) -> Result<(TestClient, FilesystemKeyStore)> {
        let (mut client, keystore) = self.into_unsynced_client().await?;

        client.sync_state().await.with_context(|| "failed to sync client state")?;

        Ok((client, keystore))
    }
}

impl Default for ClientConfig {
    /// Creates a default client config.
    ///
    /// `TEST_MIDEN_NETWORK` sets the top-level preset (defaults for all components):
    /// - `testnet`: RPC testnet, remote prover testnet, note transport testnet
    /// - `devnet`: RPC devnet, remote prover devnet, note transport devnet
    ///
    /// When unset, only RPC defaults to localhost (local prover, no note transport).
    ///
    /// Individual env vars override specific components:
    /// - `TEST_MIDEN_RPC_URL`: overrides the RPC endpoint
    /// - `TEST_MIDEN_PROVER_URL`: overrides the prover (`local` forces local prover)
    /// - `TEST_MIDEN_NOTE_TRANSPORT_URL`: overrides the note transport endpoint
    fn default() -> Self {
        let network = std::env::var("TEST_MIDEN_NETWORK").ok();
        let network_lower = network.map(|n| n.to_lowercase());

        // Resolve RPC endpoint: TEST_MIDEN_RPC_URL overrides network preset. When no network is
        // set, defaults to localhost.
        let endpoint = if let Ok(rpc_url) = std::env::var("TEST_MIDEN_RPC_URL") {
            Endpoint::try_from(rpc_url.as_str()).unwrap()
        } else {
            match network_lower.as_deref() {
                Some(NETWORK_DEVNET) => Endpoint::devnet(),
                Some(NETWORK_TESTNET) => Endpoint::testnet(),
                Some(NETWORK_LOCALHOST) | None => Endpoint::localhost(),
                Some(custom) => Endpoint::try_from(custom).unwrap(),
            }
        };

        // Resolve prover: TEST_MIDEN_PROVER_URL overrides network preset. "localhost" forces local
        // prover. Named values resolve to their URLs.
        let prover_endpoint = if let Ok(url) = std::env::var("TEST_MIDEN_PROVER_URL") {
            match url.to_lowercase().as_str() {
                NETWORK_LOCALHOST => None,
                NETWORK_DEVNET => Some(DEVNET_PROVER_ENDPOINT.to_string()),
                NETWORK_TESTNET => Some(TESTNET_PROVER_ENDPOINT.to_string()),
                _ => Some(url),
            }
        } else {
            // Network preset defaults
            match network_lower.as_deref() {
                Some(NETWORK_TESTNET) => Some(TESTNET_PROVER_ENDPOINT.to_string()),
                Some(NETWORK_DEVNET) => Some(DEVNET_PROVER_ENDPOINT.to_string()),
                _ => None,
            }
        };

        // Resolve note transport: TEST_MIDEN_NOTE_TRANSPORT_URL overrides network preset.
        let note_transport_endpoint =
            if let Ok(url) = std::env::var("TEST_MIDEN_NOTE_TRANSPORT_URL") {
                Some(url.parse::<NoteTransportEndpoint>().unwrap())
            } else {
                // Network preset defaults
                match network_lower.as_deref() {
                    Some(NETWORK_TESTNET) => Some(NoteTransportEndpoint::Testnet),
                    Some(NETWORK_DEVNET) => Some(NoteTransportEndpoint::Devnet),
                    _ => None,
                }
            };

        let timeout_ms = std::env::var("MIDEN_TEST_TIMEOUT")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(10_000);

        Self::new(endpoint, timeout_ms)
            .with_prover_endpoint(prover_endpoint)
            .with_note_transport_endpoint(note_transport_endpoint)
    }
}

/// Creates a fresh keystore directory, for a test building a client without a [`ClientConfig`].
pub fn create_test_auth_path() -> PathBuf {
    let auth_path = temp_dir().join(format!("keystore-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&auth_path).unwrap();
    auth_path
}

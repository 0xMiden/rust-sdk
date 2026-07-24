use std::env::temp_dir;
use std::fmt;
use std::path::PathBuf;
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
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_client::testing::common::{FilesystemKeyStore, TestClient, create_test_store_path};
use miden_client::{Felt, RemoteTransactionProver};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use rand::RngExt;
use uuid::Uuid;

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
            Self::Devnet => write!(f, "devnet ({})", NOTE_TRANSPORT_DEVNET_ENDPOINT),
            Self::Testnet => write!(f, "testnet ({})", NOTE_TRANSPORT_TESTNET_ENDPOINT),
            Self::Custom(url) => write!(f, "{url}"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub rpc_endpoint: Endpoint,
    pub rpc_timeout_ms: u64,
    pub store_config: PathBuf,
    pub auth_path: PathBuf,
    /// Optional remote prover endpoint. If set, the client will use a remote prover instead of the
    /// default local prover.
    pub prover_endpoint: Option<String>,
    /// Optional note transport endpoint. If set, the client will connect to a note transport
    /// service.
    pub note_transport_endpoint: Option<NoteTransportEndpoint>,
}

impl ClientConfig {
    pub fn new(rpc_endpoint: Endpoint, rpc_timeout_ms: u64) -> Self {
        Self {
            rpc_endpoint,
            rpc_timeout_ms,
            auth_path: create_test_auth_path(),
            store_config: create_test_store_path(),
            prover_endpoint: None,
            note_transport_endpoint: None,
        }
    }

    pub fn as_parts(&self) -> (Endpoint, u64, PathBuf, PathBuf) {
        (
            self.rpc_endpoint.clone(),
            self.rpc_timeout_ms,
            self.store_config.clone(),
            self.auth_path.clone(),
        )
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

    #[allow(clippy::return_self_not_must_use)]
    pub fn with_rpc_endpoint(mut self, rpc_endpoint: Endpoint) -> Self {
        self.rpc_endpoint = rpc_endpoint;
        self
    }

    pub fn rpc_endpoint(&self) -> Endpoint {
        self.rpc_endpoint.clone()
    }

    /// Creates a `TestClient` builder and keystore.
    ///
    /// Creates the client builder using the provided `ClientConfig`. The store uses a `SQLite`
    /// database at a temporary location determined by the store config.
    pub async fn into_client_builder(
        self,
    ) -> Result<(ClientBuilder<FilesystemKeyStore>, FilesystemKeyStore)> {
        let (rpc_endpoint, rpc_timeout, store_config, auth_path) = self.as_parts();

        let mut rng = rand::rng();
        let coin_seed: [u64; 4] = rng.random();

        let rng = RandomCoin::new(coin_seed.map(Felt::new_unchecked).into());

        let keystore = FilesystemKeyStore::new(auth_path.clone()).with_context(|| {
            format!("failed to create keystore at path: {}", auth_path.to_string_lossy())
        })?;

        let rpc_client = Arc::new(GrpcClient::new(&rpc_endpoint, rpc_timeout));

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

    /// Creates a `TestClient`.
    ///
    /// Creates the client using the provided [`ClientConfig`]. The store uses a `SQLite` database
    /// at a temporary location determined by the store config. The client is synced to the
    /// current state before being returned.
    pub async fn into_client(self) -> Result<(TestClient, FilesystemKeyStore)> {
        let (builder, keystore) = self.into_client_builder().await?;

        let mut client = builder.build().await.with_context(|| "failed to build test client")?;

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

        // Resolve RPC endpoint: TEST_MIDEN_RPC_URL overrides network preset.
        // When no network is set, defaults to localhost.
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

        // Resolve prover: TEST_MIDEN_PROVER_URL overrides network preset.
        // "localhost" forces local prover. Named values resolve to their URLs.
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

pub(crate) fn create_test_auth_path() -> PathBuf {
    let auth_path = temp_dir().join(format!("keystore-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&auth_path).unwrap();
    auth_path
}

use std::path::{Path, PathBuf};
use std::sync::Arc;

use miden_client::builder::ClientBuilder;
use miden_client::crypto::RandomCoin;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note_transport::NoteTransportClient;
use miden_client::note_transport::grpc::GrpcNoteTransportClient;
use miden_client::rpc::{Endpoint, GrpcClient, VerifyingRpcClient};
use miden_client::{Client, Felt};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use rand::RngExt;

/// Default store directory name, created in the current working directory.
pub const DEFAULT_STORE_DIR: &str = "miden-bench-store";

/// Configuration for benchmark execution
#[derive(Clone)]
pub struct BenchConfig {
    /// RPC endpoint for network benchmarks
    pub network: Endpoint,
    /// Number of benchmark iterations
    pub iterations: usize,
    /// Persistent store directory. Deploy saves the account and keystore here;
    /// transaction and expand commands reuse the same directory.
    pub store_path: PathBuf,
}

impl BenchConfig {
    /// Creates a new benchmark configuration
    pub fn new(network: Endpoint, iterations: usize, store_path: PathBuf) -> Self {
        Self { network, iterations, store_path }
    }
}

/// Creates a Miden client using the given endpoint and store directory.
///
/// The store directory should already exist. It will contain (or be populated with)
/// the `SQLite` database (`store.sqlite3`) and filesystem keystore (`keystore/`).
pub async fn create_client(
    endpoint: &Endpoint,
    store_path: &Path,
) -> anyhow::Result<Client<FilesystemKeyStore>> {
    create_client_with_transport(endpoint, store_path, None).await
}

/// Creates a Miden client, optionally wired to a note transport endpoint.
///
/// Without a transport endpoint `sync_state` skips its transport half entirely, so any benchmark
/// that means to measure it has to pass one.
pub async fn create_client_with_transport(
    endpoint: &Endpoint,
    store_path: &Path,
    transport_endpoint: Option<&str>,
) -> anyhow::Result<Client<FilesystemKeyStore>> {
    let sqlite_path = store_path.join("store.sqlite3");
    let keystore_path = store_path.join("keystore");
    std::fs::create_dir_all(&keystore_path)?;

    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();
    let rng_coin = RandomCoin::new(coin_seed.map(Felt::new_unchecked).into());

    let mut builder = ClientBuilder::new()
        .rpc(Arc::new(VerifyingRpcClient::new(GrpcClient::new(endpoint, 30_000))))
        .rng(Box::new(rng_coin))
        .sqlite_store(sqlite_path)
        .filesystem_keystore(keystore_path.to_str().expect("keystore path should be valid UTF-8"))?
        .tx_discard_delta(None);

    if let Some(transport) = transport_endpoint {
        builder = builder.note_transport(Arc::new(GrpcNoteTransportClient::new(
            transport.to_string(),
            30_000,
        )) as Arc<dyn NoteTransportClient>);
    }

    let client = builder.build().await?;

    Ok(client)
}

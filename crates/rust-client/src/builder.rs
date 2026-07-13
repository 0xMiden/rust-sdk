use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use miden_protocol::assembly::{DefaultSourceManager, SourceManagerSync};
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::{Felt, MAX_TX_EXECUTION_CYCLES, MIN_TX_EXECUTION_CYCLES};
use miden_tx::{ExecutionOptions, LocalTransactionProver};
use rand::Rng;

#[cfg(any(feature = "tonic", feature = "std"))]
use crate::alloc::string::ToString;
#[cfg(feature = "std")]
use crate::keystore::FilesystemKeyStore;
use crate::keystore::Keystore;
use crate::note_transport::NoteTransportClient;
use crate::pswap::PswapTransactionObserver;
use crate::rpc::{Endpoint, NodeRpcClient};
use crate::store::{Store, StoreError};
use crate::transaction::{TransactionObserver, TransactionProver};
use crate::{Client, ClientError, ClientRng, ClientRngBox, DebugMode, grpc_support};

// CONSTANTS
// ================================================================================================

/// The default number of blocks after which pending transactions are considered stale and
/// discarded.
const TX_DISCARD_DELTA: u32 = 20;
/// The default number of synced blocks between automatic irrelevant-block pruning runs.
const IRRELEVANT_BLOCK_PRUNE_INTERVAL: u32 = 1;
/// Whether the client should cache the current Partial MMR in memory by default.
const CACHE_PARTIAL_MMR_IN_MEMORY: bool = false;

pub use grpc_support::*;

// STORE BUILDER
// ================================================================================================

/// Allows the [`ClientBuilder`] to accept either an already built store instance or a factory for
/// deferring the store instantiation.
pub enum StoreBuilder {
    Store(Arc<dyn Store>),
    Factory(Box<dyn StoreFactory>),
}

/// Trait for building a store instance.
#[async_trait::async_trait]
pub trait StoreFactory {
    /// Returns a new store instance.
    async fn build(&self) -> Result<Arc<dyn Store>, StoreError>;
}

// CLIENT BUILDER
// ================================================================================================

/// A builder for constructing a Miden client.
///
/// This builder allows you to configure the various components required by the client, such as the
/// RPC endpoint, store, RNG, and authenticator. It is generic over the authenticator type.
///
/// ## Network-Aware Constructors
///
/// Use one of the network-specific constructors to get sensible defaults for a specific network:
/// - [`for_testnet()`](Self::for_testnet) - Pre-configured for Miden testnet
/// - [`for_devnet()`](Self::for_devnet) - Pre-configured for Miden devnet
/// - [`for_localhost()`](Self::for_localhost) - Pre-configured for local development
///
/// The builder provides defaults for:
/// - **RPC endpoint**: Automatically configured based on the network
/// - **Transaction prover**: Remote for testnet/devnet, local for localhost
/// - **RNG**: Random seed-based prover randomness
///
/// ## Components
///
/// The client requires several components to function:
///
/// - **RPC client** ([`NodeRpcClient`]): Provides connectivity to the Miden node for submitting
///   transactions, syncing state, and fetching account/note data. Configure via
///   [`rpc()`](Self::rpc) or [`grpc_client()`](Self::grpc_client).
///
/// - **Store** ([`Store`]): Provides persistence for accounts, notes, and transaction history.
///   Configure via [`store()`](Self::store).
///
/// - **RNG** ([`FeltRng`](miden_protocol::crypto::rand::FeltRng)): Provides randomness for
///   generating keys, serial numbers, and other cryptographic operations. If not provided, a random
///   seed-based RNG is created automatically. Configure via [`rng()`](Self::rng).
///
/// - **Authenticator** ([`TransactionAuthenticator`](miden_tx::auth::TransactionAuthenticator)):
///   Handles transaction signing when signatures are requested from within the VM. Configure via
///   [`authenticator()`](Self::authenticator).
///
/// - **Transaction prover** ([`TransactionProver`]): Generates proofs for transactions. Defaults to
///   a local prover if not specified. Configure via [`prover()`](Self::prover).
///
/// - **Note transport** ([`NoteTransportClient`]): Optional component for exchanging private notes
///   through the Miden note transport network. Configure via
///   [`note_transport()`](Self::note_transport).
///
/// - **Debug mode**: Enables debug mode for transaction execution. Configure via
///   [`in_debug_mode()`](Self::in_debug_mode).
///
/// - **Transaction discard delta**: Number of blocks after which pending transactions are
///   considered stale and discarded. Configure via [`tx_discard_delta()`](Self::tx_discard_delta).
///
/// - **In-memory Partial MMR cache**: Reuses the current partial blockchain MMR instead of
///   rebuilding it from store. Disabled by default. Configure via
///   [`cache_partial_mmr_in_memory()`](Self::cache_partial_mmr_in_memory).
///
/// - **Max block number delta**: Maximum number of blocks the client can be behind the network for
///   transactions and account proofs to be considered valid. Configure via
///   [`max_block_number_delta()`](Self::max_block_number_delta).
pub struct ClientBuilder<AUTH> {
    /// An optional custom RPC client. If provided, this takes precedence over `rpc_endpoint`.
    rpc_api: Option<Arc<dyn NodeRpcClient>>,
    /// An optional store provided by the user.
    pub store: Option<StoreBuilder>,
    /// An optional RNG provided by the user.
    rng: Option<ClientRngBox>,
    /// The authenticator provided by the user.
    authenticator: Option<Arc<AUTH>>,
    /// A flag to enable debug mode.
    in_debug_mode: DebugMode,
    /// Number of blocks after which pending transactions are considered stale and discarded.
    /// If `None`, there is no limit and transactions will be kept indefinitely.
    tx_discard_delta: Option<u32>,
    /// Number of synced blocks between automatic pruning runs for irrelevant block data.
    /// If `None`, automatic irrelevant-block pruning is disabled.
    irrelevant_block_prune_interval: Option<u32>,
    /// Whether the current Partial MMR should be cached in memory between sync-related operations.
    cache_partial_mmr_in_memory: bool,
    /// Maximum number of blocks the client can be behind the network for transactions and account
    /// proofs to be considered valid.
    max_block_number_delta: Option<u32>,
    /// An optional custom note transport client.
    note_transport_api: Option<Arc<dyn NoteTransportClient>>,
    /// Configuration for lazy note transport initialization (used by network constructors).
    #[allow(unused)]
    note_transport_config: Option<NoteTransportConfig>,
    /// An optional custom transaction prover.
    tx_prover: Option<Arc<dyn TransactionProver + Send + Sync>>,
    /// The endpoint used by the builder for network configuration.
    endpoint: Option<Endpoint>,
    /// An optional shared source manager for MASM source information.
    source_manager: Option<Arc<dyn SourceManagerSync>>,
}

impl<AUTH> Default for ClientBuilder<AUTH> {
    fn default() -> Self {
        Self {
            rpc_api: None,
            store: None,
            rng: None,
            authenticator: None,
            in_debug_mode: DebugMode::Disabled,
            tx_discard_delta: Some(TX_DISCARD_DELTA),
            irrelevant_block_prune_interval: Some(IRRELEVANT_BLOCK_PRUNE_INTERVAL),
            cache_partial_mmr_in_memory: CACHE_PARTIAL_MMR_IN_MEMORY,
            max_block_number_delta: None,
            note_transport_api: None,
            note_transport_config: None,
            tx_prover: None,
            endpoint: None,
            source_manager: None,
        }
    }
}

/// Network-specific constructors for [`ClientBuilder`].
///
/// These constructors automatically configure the builder for a specific network,
/// including RPC endpoint, transaction prover, and note transport (where applicable).
#[cfg(feature = "tonic")]
impl<AUTH> ClientBuilder<AUTH>
where
    AUTH: BuilderAuthenticator,
{
    /// Creates a `ClientBuilder` pre-configured for Miden testnet.
    ///
    /// This automatically configures:
    /// - **RPC**: [`Endpoint::testnet()`]
    /// - **Prover**: Remote prover at [`TESTNET_PROVER_ENDPOINT`]
    /// - **Note transport**:
    ///   [`NOTE_TRANSPORT_TESTNET_ENDPOINT`](crate::note_transport::NOTE_TRANSPORT_TESTNET_ENDPOINT)
    ///
    /// You still need to provide:
    /// - A store (via `.store()`)
    /// - An authenticator (via `.authenticator()`)
    ///
    /// All defaults can be overridden by calling the corresponding builder methods
    /// after `for_testnet()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = ClientBuilder::for_testnet()
    ///     .store(store)
    ///     .authenticator(Arc::new(keystore))
    ///     .build()
    ///     .await?;
    /// ```
    #[must_use]
    pub fn for_testnet() -> Self {
        let endpoint = Endpoint::testnet();
        Self {
            rpc_api: Some(Arc::new(crate::rpc::GrpcClient::new(
                &endpoint,
                DEFAULT_GRPC_TIMEOUT_MS,
            ))),
            tx_prover: Some(Arc::new(RemoteTransactionProver::new(
                TESTNET_PROVER_ENDPOINT.to_string(),
            ))),
            note_transport_config: Some(NoteTransportConfig {
                endpoint: crate::note_transport::NOTE_TRANSPORT_TESTNET_ENDPOINT.to_string(),
                timeout_ms: DEFAULT_GRPC_TIMEOUT_MS,
            }),
            endpoint: Some(endpoint),
            ..Self::default()
        }
    }

    /// Creates a `ClientBuilder` pre-configured for Miden devnet.
    ///
    /// This automatically configures:
    /// - **RPC**: [`Endpoint::devnet()`]
    /// - **Prover**: Remote prover at [`DEVNET_PROVER_ENDPOINT`]
    /// - **Note transport**:
    ///   [`NOTE_TRANSPORT_DEVNET_ENDPOINT`](crate::note_transport::NOTE_TRANSPORT_DEVNET_ENDPOINT)
    ///
    /// You still need to provide:
    /// - A store (via `.store()`)
    /// - An authenticator (via `.authenticator()`)
    ///
    /// All defaults can be overridden by calling the corresponding builder methods
    /// after `for_devnet()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = ClientBuilder::for_devnet()
    ///     .store(store)
    ///     .authenticator(Arc::new(keystore))
    ///     .build()
    ///     .await?;
    /// ```
    #[must_use]
    pub fn for_devnet() -> Self {
        let endpoint = Endpoint::devnet();
        Self {
            rpc_api: Some(Arc::new(crate::rpc::GrpcClient::new(
                &endpoint,
                DEFAULT_GRPC_TIMEOUT_MS,
            ))),
            tx_prover: Some(Arc::new(RemoteTransactionProver::new(
                DEVNET_PROVER_ENDPOINT.to_string(),
            ))),
            note_transport_config: Some(NoteTransportConfig {
                endpoint: crate::note_transport::NOTE_TRANSPORT_DEVNET_ENDPOINT.to_string(),
                timeout_ms: DEFAULT_GRPC_TIMEOUT_MS,
            }),
            endpoint: Some(endpoint),
            ..Self::default()
        }
    }

    /// Creates a `ClientBuilder` pre-configured for localhost.
    ///
    /// This automatically configures:
    /// - **RPC**: `http://localhost:57291`
    /// - **Prover**: Local (default)
    ///
    /// Note transport is not configured by default for localhost.
    ///
    /// You still need to provide:
    /// - A store (via `.store()`)
    /// - An authenticator (via `.authenticator()`)
    ///
    /// All defaults can be overridden by calling the corresponding builder methods
    /// after `for_localhost()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = ClientBuilder::for_localhost()
    ///     .store(store)
    ///     .authenticator(Arc::new(keystore))
    ///     .build()
    ///     .await?;
    /// ```
    #[must_use]
    pub fn for_localhost() -> Self {
        let endpoint = Endpoint::localhost();
        Self {
            rpc_api: Some(Arc::new(crate::rpc::GrpcClient::new(
                &endpoint,
                DEFAULT_GRPC_TIMEOUT_MS,
            ))),
            endpoint: Some(endpoint),
            ..Self::default()
        }
    }
}

impl<AUTH> ClientBuilder<AUTH>
where
    AUTH: BuilderAuthenticator,
{
    /// Create a new `ClientBuilder` with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable or disable debug mode.
    #[must_use]
    pub fn in_debug_mode(mut self, debug: DebugMode) -> Self {
        self.in_debug_mode = debug;
        self
    }

    /// Sets a custom RPC client directly.
    #[must_use]
    pub fn rpc(mut self, client: Arc<dyn NodeRpcClient>) -> Self {
        self.rpc_api = Some(client);
        self
    }

    /// Sets a gRPC client from the endpoint and optional timeout.
    #[must_use]
    #[cfg(feature = "tonic")]
    pub fn grpc_client(mut self, endpoint: &crate::rpc::Endpoint, timeout_ms: Option<u64>) -> Self {
        self.rpc_api = Some(Arc::new(crate::rpc::GrpcClient::new(
            endpoint,
            timeout_ms.unwrap_or(DEFAULT_GRPC_TIMEOUT_MS),
        )));
        self
    }

    /// Provide a store to be used by the client.
    #[must_use]
    pub fn store(mut self, store: Arc<dyn Store>) -> Self {
        self.store = Some(StoreBuilder::Store(store));
        self
    }

    /// Optionally provide a custom RNG.
    #[must_use]
    pub fn rng(mut self, rng: ClientRngBox) -> Self {
        self.rng = Some(rng);
        self
    }

    /// Optionally provide a custom authenticator instance.
    #[must_use]
    pub fn authenticator(mut self, authenticator: Arc<AUTH>) -> Self {
        self.authenticator = Some(authenticator);
        self
    }

    /// Overrides the source manager used to retain MASM source information for assembled programs.
    ///
    /// If not set, the client uses a default [`DefaultSourceManager`]. The same instance is
    /// forwarded to the transaction executor and to every script compiled through the client
    /// (e.g. via [`Client::code_builder`](crate::Client::code_builder)).
    ///
    /// Set this explicitly only when scripts or modules are compiled outside the client (for
    /// example, using an external [`Assembler`](miden_protocol::assembly::Assembler)): pass the
    /// same `Arc` used by that external assembler so all source spans resolve correctly at
    /// runtime.
    #[must_use]
    pub fn source_manager(mut self, sm: Arc<dyn SourceManagerSync>) -> Self {
        self.source_manager = Some(sm);
        self
    }

    /// Optionally set a maximum number of blocks that the client can be behind the network.
    /// By default, there's no maximum.
    #[must_use]
    pub fn max_block_number_delta(mut self, delta: u32) -> Self {
        self.max_block_number_delta = Some(delta);
        self
    }

    /// Sets the number of blocks after which pending transactions are considered stale and
    /// discarded.
    ///
    /// If a transaction has not been included in a block within this many blocks after submission,
    /// it will be discarded. If `None`, transactions will be kept indefinitely.
    ///
    /// By default, the delta is set to `TX_DISCARD_DELTA` (20 blocks).
    #[must_use]
    pub fn tx_discard_delta(mut self, delta: Option<u32>) -> Self {
        self.tx_discard_delta = delta;
        self
    }

    /// Sets the number of synced blocks between automatic irrelevant-block pruning runs.
    ///
    /// Values defer pruning until the client has advanced by at least that many sync blocks since
    /// the last prune. `None` disables automatic pruning entirely.
    #[must_use]
    pub fn irrelevant_block_prune_interval(mut self, interval: Option<u32>) -> Self {
        self.irrelevant_block_prune_interval = interval;
        self
    }

    /// Enables or disables the in-memory Partial MMR cache.
    ///
    /// When enabled, the client reuses the current Partial MMR between sync and pruning
    /// operations. When disabled, it rebuilds the Partial MMR from the store each time it is
    /// needed.
    #[must_use]
    pub fn cache_partial_mmr_in_memory(mut self, enabled: bool) -> Self {
        self.cache_partial_mmr_in_memory = enabled;
        self
    }

    /// Sets the number of blocks after which pending transactions are considered stale and
    /// discarded.
    ///
    /// This is an alias for [`tx_discard_delta`](Self::tx_discard_delta).
    #[deprecated(since = "0.10.0", note = "Use `tx_discard_delta` instead")]
    #[must_use]
    pub fn tx_graceful_blocks(mut self, delta: Option<u32>) -> Self {
        self.tx_discard_delta = delta;
        self
    }

    /// Sets a custom note transport client directly.
    #[must_use]
    pub fn note_transport(mut self, client: Arc<dyn NoteTransportClient>) -> Self {
        self.note_transport_api = Some(client);
        self
    }

    /// Sets a custom transaction prover.
    #[must_use]
    pub fn prover(mut self, prover: Arc<dyn TransactionProver + Send + Sync>) -> Self {
        self.tx_prover = Some(prover);
        self
    }

    /// Returns the endpoint configured for this builder, if any.
    ///
    /// This is set automatically when using network-specific constructors like
    /// [`for_testnet()`](Self::for_testnet), [`for_devnet()`](Self::for_devnet),
    /// or [`for_localhost()`](Self::for_localhost).
    #[must_use]
    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref()
    }

    /// Build and return the `Client`.
    ///
    /// # Errors
    ///
    /// - Returns an error if no RPC client was provided.
    /// - Returns an error if the store cannot be instantiated.
    #[allow(clippy::unused_async, unused_mut)]
    pub async fn build(mut self) -> Result<Client<AUTH>, ClientError> {
        // Determine the RPC client to use.
        let rpc_api: Arc<dyn NodeRpcClient> = if let Some(client) = self.rpc_api {
            client
        } else {
            return Err(ClientError::ClientInitializationError(
                "RPC client is required. Call `.rpc(...)` or `.grpc_client(...)`.".into(),
            ));
        };

        // Ensure a store was provided.
        let store = if let Some(store_builder) = self.store {
            match store_builder {
                StoreBuilder::Store(store) => store,
                StoreBuilder::Factory(factory) => factory.build().await?,
            }
        } else {
            return Err(ClientError::ClientInitializationError(
                "Store must be specified. Call `.store(...)`.".into(),
            ));
        };

        // Use the provided RNG, or create a default one.
        let rng = if let Some(user_rng) = self.rng {
            user_rng
        } else {
            let mut seed_rng = rand::rng();
            let coin_seed: [u64; 4] = seed_rng.random();
            Box::new(RandomCoin::new(coin_seed.map(Felt::new_unchecked).into()))
        };

        // Set default prover if not provided
        let tx_prover: Arc<dyn TransactionProver + Send + Sync> =
            self.tx_prover.unwrap_or_else(|| Arc::new(LocalTransactionProver::default()));

        // Use the provided source manager, or create a default one.
        let source_manager: Arc<dyn SourceManagerSync> =
            self.source_manager.unwrap_or_else(|| Arc::new(DefaultSourceManager::default()));

        // Initialize genesis commitment in RPC client
        if let Some((genesis, _)) = store.get_block_header_by_num(BlockNumber::GENESIS).await? {
            rpc_api.set_genesis_commitment(genesis.commitment()).await?;
        }

        // Set the RPC client with persisted limits if available.
        // If not present, they will be fetched from the node during sync_state.
        if let Some(limits) = store.get_rpc_limits().await? {
            rpc_api.set_rpc_limits(limits).await;
        }

        // Initialize note transport: prefer explicit client, fall back to config (tonic only)
        #[cfg(feature = "tonic")]
        if self.note_transport_api.is_none()
            && let Some(config) = self.note_transport_config
        {
            let transport = crate::note_transport::grpc::GrpcNoteTransportClient::new(
                config.endpoint,
                config.timeout_ms,
            );

            self.note_transport_api = Some(Arc::new(transport) as Arc<dyn NoteTransportClient>);
        }

        // Built-in transaction observers fired by `apply_transaction`.
        // Additional observers can be attached via
        // `Client::with_transaction_observer`.
        let transaction_observers: Vec<Arc<dyn TransactionObserver>> =
            vec![Arc::new(PswapTransactionObserver::new(store.clone()))];

        // Construct and return the Client
        Ok(Client {
            store,
            rng: ClientRng::new(rng),
            rpc_api,
            tx_prover,
            authenticator: self.authenticator,
            source_manager,
            exec_options: ExecutionOptions::new(
                Some(MAX_TX_EXECUTION_CYCLES),
                MIN_TX_EXECUTION_CYCLES,
                ExecutionOptions::DEFAULT_CORE_TRACE_FRAGMENT_SIZE,
                false,
                self.in_debug_mode.into(),
            )
            .expect("Default executor's options should always be valid"),
            tx_discard_delta: self.tx_discard_delta,
            irrelevant_block_prune_interval: self.irrelevant_block_prune_interval,
            last_irrelevant_block_prune_sync_height: None,
            max_block_number_delta: self.max_block_number_delta,
            note_transport_api: self.note_transport_api.clone(),
            cache_partial_mmr_in_memory: self.cache_partial_mmr_in_memory,
            partial_mmr: None,
            transaction_observers,
        })
    }
}

// FILESYSTEM KEYSTORE CONVENIENCE METHOD
// ================================================================================================

/// Marker trait to capture the bounds the builder requires for the authenticator type
/// parameter.
#[cfg(feature = "std")]
pub trait BuilderAuthenticator: Keystore + From<FilesystemKeyStore> + 'static {}
#[cfg(feature = "std")]
impl<T> BuilderAuthenticator for T where T: Keystore + From<FilesystemKeyStore> + 'static {}

#[cfg(not(feature = "std"))]
pub trait BuilderAuthenticator: Keystore + 'static {}
#[cfg(not(feature = "std"))]
impl<T> BuilderAuthenticator for T where T: Keystore + 'static {}

/// Convenience method for [`ClientBuilder`] when using [`FilesystemKeyStore`] as the authenticator.
#[cfg(feature = "std")]
impl ClientBuilder<FilesystemKeyStore> {
    /// Creates a [`FilesystemKeyStore`] from the given path and sets it as the authenticator.
    ///
    /// This is a convenience method that creates the keystore and configures it as the
    /// authenticator in a single call. The keystore provides transaction signing capabilities
    /// using keys stored on the filesystem.
    ///
    /// # Errors
    ///
    /// Returns an error if the keystore cannot be created from the given path.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = ClientBuilder::new()
    ///     .rpc(rpc_client)
    ///     .store(store)
    ///     .filesystem_keystore("path/to/keys")?
    ///     .build()
    ///     .await?;
    /// ```
    pub fn filesystem_keystore(
        self,
        keystore_path: impl Into<std::path::PathBuf>,
    ) -> Result<Self, ClientError> {
        let keystore = FilesystemKeyStore::new(keystore_path.into())
            .map_err(|e| ClientError::ClientInitializationError(e.to_string()))?;
        Ok(self.authenticator(Arc::new(keystore)))
    }
}

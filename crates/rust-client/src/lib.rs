#![cfg_attr(docsrs, feature(doc_cfg))]

//! A no_std-compatible client library for interacting with the Miden network.
//!
//! This crate provides a lightweight client that handles connections to the Miden node, manages
//! accounts and their state, and facilitates executing, proving, and submitting transactions.
//!
//! For a protocol-level overview and guides for getting started, please visit the official
//! [Miden docs](https://docs.miden.xyz/).
//!
//! ## Overview
//!
//! The library is organized into several key modules:
//!
//! - **Accounts:** Provides types for managing accounts. Once accounts are tracked by the client,
//!   their state is updated with every transaction and validated during each sync.
//!
//! - **Notes:** Contains types and utilities for working with notes in the Miden client.
//!
//! - **RPC:** Facilitates communication with Miden node, exposing RPC methods for syncing state,
//!   fetching block headers, and submitting transactions.
//!
//! - **Store:** Defines and implements the persistence layer for accounts, transactions, notes, and
//!   other entities.
//!
//! - **Sync:** Provides functionality to synchronize the local state with the current state on the
//!   Miden network.
//!
//! - **Transactions:** Offers capabilities to build, execute, prove, and submit transactions.
//!
//! Additionally, the crate re-exports several utility modules:
//!
//! - **Assembly:** Types for working with Miden Assembly.
//! - **Assets:** Types and utilities for working with assets.
//! - **Auth:** Authentication-related types and functionalities.
//! - **Blocks:** Types for handling block headers.
//! - **Crypto:** Cryptographic types and utilities, including random number generators.
//! - **Utils:** Miscellaneous utilities for serialization and common operations.
//! - **`AggLayer`:** Bridge account components, note constructors, and Ethereum-compatible helper
//!   types from the Miden `AggLayer` protocol crate.
//!
//! The library is designed to work in both `no_std` and `std` environments and is
//! configurable via Cargo features.
//!
//! ## Usage
//!
//! To use the Miden client library in your project, add it as a dependency in your `Cargo.toml`:
//!
//! ```toml
//! [dependencies]
//! miden-client = "0.10"
//! ```
//!
//! ## Example
//!
//! Below is a brief example illustrating how to instantiate the client using `ClientBuilder`:
//!
//! ```rust,ignore
//! use std::sync::Arc;
//!
//! use miden_client::DebugMode;
//! use miden_client::builder::ClientBuilder;
//! use miden_client::keystore::FilesystemKeyStore;
//! use miden_client::rpc::{Endpoint, GrpcClient};
//! use miden_client_sqlite_store::SqliteStore;
//!
//! # pub async fn create_test_client() -> Result<(), Box<dyn std::error::Error>> {
//! // Create the SQLite store.
//! let sqlite_store = SqliteStore::new("path/to/store".try_into()?).await?;
//! let store = Arc::new(sqlite_store);
//!
//! // Create the keystore for transaction signing.
//! let keystore = FilesystemKeyStore::new("path/to/keys/directory".try_into()?)?;
//!
//! // Create the RPC client.
//! let endpoint = Endpoint::new("https".into(), "localhost".into(), Some(57291));
//!
//! // Instantiate the client using the builder.
//! let client = ClientBuilder::new()
//!     .rpc(Arc::new(GrpcClient::new(&endpoint, 10_000)))
//!     .store(store)
//!     .authenticator(Arc::new(keystore))
//!     .in_debug_mode(DebugMode::Disabled)
//!     .build()
//!     .await?;
//!
//! # Ok(())
//! # }
//! ```
//!
//! For network-specific defaults, use the convenience constructors:
//!
//! ```ignore
//! // For testnet (includes remote prover and note transport)
//! let client = ClientBuilder::for_testnet()
//!     .store(store)
//!     .authenticator(Arc::new(keystore))
//!     .build()
//!     .await?;
//!
//! // For local development
//! let client = ClientBuilder::for_localhost()
//!     .store(store)
//!     .authenticator(Arc::new(keystore))
//!     .build()
//!     .await?;
//! ```
//!
//! For additional usage details, configuration options, and examples, consult the documentation for
//! each module.

#![no_std]

#[macro_use]
extern crate alloc;
use alloc::boxed::Box;

#[cfg(feature = "std")]
extern crate std;

pub mod account;
pub mod grpc_support;
pub mod keystore;
pub mod note;
pub mod note_transport;
pub mod pswap;
pub mod rpc;
pub mod settings;
pub mod store;
pub mod sync;
pub mod transaction;
pub mod utils;

pub mod builder;

#[cfg(feature = "testing")]
mod test_utils;

pub mod errors;

pub use miden_protocol::utils::serde::{Deserializable, Serializable, SliceReader};

// RE-EXPORTS
// ================================================================================================

pub mod notes {
    pub use miden_protocol::note::NoteFile;
}

/// Provides `AggLayer` bridge components, note constructors, and helper types.
pub mod agglayer {
    pub use miden_agglayer::*;
}

/// Provides types and utilities for working with Miden Assembly.
pub mod assembly {
    pub use miden_protocol::MastForest;
    pub use miden_protocol::assembly::debuginfo::SourceManagerSync;
    #[cfg(feature = "std")]
    pub use miden_protocol::assembly::debuginfo::{SourceManagerExt, Uri};
    pub use miden_protocol::assembly::diagnostics::Report;
    pub use miden_protocol::assembly::diagnostics::reporting::PrintDiagnostic;
    pub use miden_protocol::assembly::mast::MastNodeExt;
    pub use miden_protocol::assembly::{
        Assembler,
        DefaultSourceManager,
        Library,
        Module,
        ModuleKind,
        Path,
    };
    pub use miden_standards::code_builder::CodeBuilder;
}

/// Provides types and utilities for working with assets within the Miden network.
pub mod asset {
    pub use miden_protocol::account::delta::{
        AccountStorageDelta,
        AccountVaultDelta,
        FungibleAssetDelta,
        NonFungibleAssetDelta,
        NonFungibleDeltaAction,
        StorageMapDelta,
        StorageSlotDelta,
    };
    pub use miden_protocol::account::{
        AccountStorageHeader,
        StorageMapWitness,
        StorageSlotContent,
        StorageSlotHeader,
    };
    pub use miden_protocol::asset::{
        Asset,
        AssetAmount,
        AssetCallbackFlag,
        AssetCallbacks,
        AssetComposition,
        AssetId,
        AssetVault,
        AssetVaultKey,
        AssetWitness,
        FungibleAsset,
        NonFungibleAsset,
        NonFungibleAssetDetails,
        PartialVault,
        TokenSymbol,
    };
}

/// Provides authentication-related types and functionalities for the Miden
/// network.
pub mod auth {
    pub use miden_protocol::account::auth::{
        AuthScheme as AuthSchemeId,
        AuthSecretKey,
        PublicKey,
        PublicKeyCommitment,
        Signature,
    };
    pub use miden_standards::AuthMethod;
    pub use miden_standards::account::auth::{
        AuthMultisig,
        AuthMultisigConfig,
        AuthSingleSig,
        AuthSingleSigAcl,
        AuthSingleSigAclConfig,
        NoAuth,
    };
    pub use miden_tx::auth::{BasicAuthenticator, SigningInputs, TransactionAuthenticator};

    pub use crate::account::component::AuthScheme;

    pub const RPO_FALCON_SCHEME_ID: AuthSchemeId = AuthSchemeId::Falcon512Poseidon2;
    pub const ECDSA_K256_KECCAK_SCHEME_ID: AuthSchemeId = AuthSchemeId::EcdsaK256Keccak;
}

/// Provides types for working with blocks within the Miden network.
pub mod block {
    pub use miden_protocol::block::{BlockHeader, BlockNumber};
}

/// Provides cryptographic types and utilities used within the Miden rollup
/// network. It re-exports commonly used types and random number generators like `FeltRng` from
/// the `miden_standards` crate.
pub mod crypto {
    pub mod rpo_falcon512 {
        pub use miden_protocol::crypto::dsa::falcon512_poseidon2::{
            PublicKey,
            SecretKey,
            Signature,
        };
    }
    pub use miden_protocol::crypto::hash::blake::Blake3Digest;
    pub use miden_protocol::crypto::hash::poseidon2::Poseidon2;
    pub use miden_protocol::crypto::hash::rpo::Rpo256;
    pub use miden_protocol::crypto::merkle::mmr::{
        Forest,
        InOrderIndex,
        MmrDelta,
        MmrPeaks,
        MmrProof,
        PartialMmr,
    };
    pub use miden_protocol::crypto::merkle::smt::{
        LeafIndex,
        SMT_DEPTH,
        Smt,
        SmtForest,
        SmtLeaf,
        SmtProof,
    };
    pub use miden_protocol::crypto::merkle::store::MerkleStore;
    pub use miden_protocol::crypto::merkle::{
        EmptySubtreeRoots,
        MerkleError,
        MerklePath,
        MerkleTree,
        NodeIndex,
        SparseMerklePath,
    };
    pub use miden_protocol::crypto::rand::{FeltRng, RandomCoin};
}

/// Provides types for working with addresses within the Miden network.
pub mod address {
    pub use miden_protocol::address::{
        Address,
        AddressId,
        AddressInterface,
        CustomNetworkId,
        NetworkId,
        RoutingParameters,
    };
}

/// Provides types for working with the virtual machine within the Miden network.
pub mod vm {
    pub use miden_protocol::vm::{
        AdviceInputs,
        AdviceMap,
        AttributeSet,
        MIN_STACK_DEPTH,
        Package,
        PackageExport,
        PackageManifest,
        ProcedureExport,
        Program,
        QualifiedProcedureName,
        Section,
        SectionId,
        TargetType,
    };
}

pub use async_trait::async_trait;
pub use errors::*;
use miden_protocol::assembly::SourceManagerSync;
pub use miden_protocol::{
    EMPTY_WORD,
    Felt,
    MAX_TX_EXECUTION_CYCLES,
    MIN_TX_EXECUTION_CYCLES,
    ONE,
    PrettyPrint,
    WORD_SIZE,
    Word,
    ZERO,
};
pub use miden_remote_prover_client::RemoteTransactionProver;
pub use miden_tx::ExecutionOptions;

/// Provides test utilities for working with accounts and account IDs
/// within the Miden network. This module is only available when the `testing` feature is
/// enabled.
#[cfg(feature = "testing")]
pub mod testing {
    pub use miden_protocol::testing::account_id;
    /// Raw access to `miden-standards` testing modules for items not curated by
    /// `miden-client`.
    pub use miden_standards::testing as standards;
    pub use miden_standards::testing::note::NoteBuilder;
    pub use miden_testing::*;

    pub use crate::test_utils::*;
}

use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::merkle::mmr::PartialMmr;
use miden_protocol::crypto::rand::FeltRng;
use miden_tx::auth::TransactionAuthenticator;
use rand::RngCore;
use rpc::NodeRpcClient;
use store::Store;

use crate::note_transport::NoteTransportClient;
use crate::transaction::TransactionProver;

// MIDEN CLIENT
// ================================================================================================

/// A light client for connecting to the Miden network.
///
/// Miden client is responsible for managing a set of accounts. Specifically, the client:
/// - Keeps track of the current and historical states of a set of accounts and related objects such
///   as notes and transactions.
/// - Connects to a Miden node to periodically sync with the current state of the network.
/// - Executes, proves, and submits transactions to the network as directed by the user.
pub struct Client<AUTH> {
    /// The client's store, which provides a way to write and read entities to provide persistence.
    store: Arc<dyn Store>,
    /// An instance of [`FeltRng`] which provides randomness tools for generating new keys,
    /// serial numbers, etc.
    rng: ClientRng,
    /// An instance of [`NodeRpcClient`] which provides a way for the client to connect to the
    /// Miden node.
    rpc_api: Arc<dyn NodeRpcClient>,
    /// An instance of a [`TransactionProver`] which will be the default prover for the
    /// client.
    tx_prover: Arc<dyn TransactionProver + Send + Sync>,
    /// An instance of a [`TransactionAuthenticator`] which will be used by the transaction
    /// executor whenever a signature is requested from within the VM.
    authenticator: Option<Arc<AUTH>>,
    /// Shared source manager used to retain MASM source information for assembled programs.
    source_manager: Arc<dyn SourceManagerSync>,
    /// Options that control the transaction executor's runtime behaviour (e.g. debug mode).
    exec_options: ExecutionOptions,
    /// Number of blocks after which pending transactions are considered stale and discarded.
    tx_discard_delta: Option<u32>,
    /// Number of synced blocks between automatic irrelevant-block pruning runs.
    irrelevant_block_prune_interval: Option<u32>,
    /// Sync height at which the last automatic irrelevant-block prune completed.
    last_irrelevant_block_prune_sync_height: Option<BlockNumber>,
    /// Maximum number of blocks the client can be behind the network for transactions and account
    /// proofs to be considered valid.
    max_block_number_delta: Option<u32>,
    /// An instance of [`NoteTransportClient`] which provides a way for the client to connect to
    /// the Miden Note Transport network.
    note_transport_api: Option<Arc<dyn NoteTransportClient>>,
    /// Whether the client should cache the current Partial MMR in memory.
    cache_partial_mmr_in_memory: bool,
    /// Cached [`PartialMmr`] for the chain's MMR. Lazily built from the store and kept in sync
    /// across sync/prune operations. `None` forces a rebuild on next access.
    partial_mmr: Option<CachedPartialMmr>,
    /// Observers fired by `apply_transaction`. See
    /// [`Client::with_transaction_observer`].
    transaction_observers: Vec<Arc<dyn transaction::TransactionObserver>>,
}

/// Cached [`PartialMmr`] with a two-part freshness fingerprint:
///
/// - `store_peaks_hash`: peaks at the current sync height - guards against chain/height drift.
/// - `tracked_blocks_hash`: hash of the store's tracked block numbers - guards against drift
///   between store-tracked and cache-tracked blocks. Required because a same-height update can mark
///   an existing block relevant without changing peaks; pruning the cached MMR while it's missing
///   such a block would over-delete auth nodes that the store still needs.
///
/// The cached MMR includes the sync-height block as a tracked leaf; the store persists the
/// peaks committed by that block's header, i.e. the peaks over the chain *before* that block
/// was added, so the two states are offset by one leaf.
pub(crate) struct CachedPartialMmr {
    pub(crate) store_peaks_hash: Word,
    pub(crate) tracked_blocks_hash: Word,
    pub(crate) mmr: PartialMmr,
}

/// Constructors.
impl<AUTH> Client<AUTH>
where
    AUTH: builder::BuilderAuthenticator,
{
    /// Returns a new [`ClientBuilder`](builder::ClientBuilder) for constructing a client.
    ///
    /// This is a convenience method equivalent to calling `ClientBuilder::new()`.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let client = Client::builder()
    ///     .rpc(rpc_client)
    ///     .store(store)
    ///     .authenticator(Arc::new(keystore))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn builder() -> builder::ClientBuilder<AUTH> {
        builder::ClientBuilder::new()
    }
}

/// Access methods.
impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator,
{
    /// Returns true if the client is in debug mode.
    pub fn in_debug_mode(&self) -> bool {
        self.exec_options.enable_debugging()
    }

    /// Returns an instance of the `CodeBuilder`
    pub fn code_builder(&self) -> assembly::CodeBuilder {
        assembly::CodeBuilder::with_source_manager(self.source_manager.clone())
    }

    /// Returns an instance of [`note::NoteScreener`] configured for this client.
    pub fn note_screener(&self) -> note::NoteScreener {
        note::NoteScreener::new(self.store.clone(), self.rpc_api.clone())
    }

    /// Returns a reference to the client's random number generator. This can be used to generate
    /// randomness for various purposes such as serial numbers, keys, etc.
    pub fn rng(&mut self) -> &mut ClientRng {
        &mut self.rng
    }

    pub fn prover(&self) -> Arc<dyn TransactionProver + Send + Sync> {
        self.tx_prover.clone()
    }

    pub fn authenticator(&self) -> Option<&Arc<AUTH>> {
        self.authenticator.as_ref()
    }

    /// Returns the shared source manager used to retain MASM source information for assembled
    /// programs.
    pub fn source_manager(&self) -> Arc<dyn SourceManagerSync> {
        self.source_manager.clone()
    }
}

impl<AUTH> Client<AUTH> {
    /// Returns the identifier of the underlying store (e.g. `IndexedDB` database name, `SQLite`
    /// file path).
    pub fn store_identifier(&self) -> &str {
        self.store.identifier()
    }

    /// Registers a [`transaction::TransactionObserver`]. Per-observer failures are logged.
    pub fn with_transaction_observer(
        &mut self,
        observer: Arc<dyn transaction::TransactionObserver>,
    ) {
        self.transaction_observers.push(observer);
    }

    /// Returns the network ID of the node the client is connected to.
    pub async fn network_id(&self) -> Result<address::NetworkId, ClientError> {
        Ok(self.rpc_api.get_network_id().await?)
    }

    // TEST HELPERS
    // --------------------------------------------------------------------------------------------

    #[cfg(any(test, feature = "testing"))]
    pub fn test_rpc_api(&mut self) -> &mut Arc<dyn NodeRpcClient> {
        &mut self.rpc_api
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn test_store(&mut self) -> &mut Arc<dyn Store> {
        &mut self.store
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn test_has_cached_partial_mmr(&self) -> bool {
        self.partial_mmr.is_some()
    }
}

// CLIENT RNG
// ================================================================================================

// NOTE: The idea of having `ClientRng` is to enforce `Send` and `Sync` over `FeltRng`.
// This allows `Client`` to be `Send` and `Sync`. There may be users that would want to use clients
// with !Send/!Sync RNGs. For this we have two options:
//
// - We can make client generic over R (adds verbosity but is more flexible and maybe even correct)
// - We can optionally (e.g., based on features/target) change `ClientRng` definition to not enforce
//   these bounds. (similar to TransactionAuthenticator)

/// Marker trait for RNGs that can be shared across threads and used by the client.
pub trait ClientFeltRng: FeltRng + Send + Sync {}
impl<T> ClientFeltRng for T where T: FeltRng + Send + Sync {}

/// Boxed RNG trait object used by the client.
pub type ClientRngBox = Box<dyn ClientFeltRng>;

/// A wrapper around a [`FeltRng`] that implements the [`RngCore`] trait.
/// This allows the user to pass their own generic RNG so that it's used by the client.
pub struct ClientRng(ClientRngBox);

impl ClientRng {
    pub fn new(rng: ClientRngBox) -> Self {
        Self(rng)
    }

    pub fn inner_mut(&mut self) -> &mut ClientRngBox {
        &mut self.0
    }
}

impl RngCore for ClientRng {
    fn next_u32(&mut self) -> u32 {
        self.0.next_u32()
    }

    fn next_u64(&mut self) -> u64 {
        self.0.next_u64()
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        self.0.fill_bytes(dest);
    }
}

impl FeltRng for ClientRng {
    fn draw_element(&mut self) -> Felt {
        self.0.draw_element()
    }

    fn draw_word(&mut self) -> Word {
        self.0.draw_word()
    }
}

/// Indicates whether the client is operating in debug mode.
#[derive(Debug, Clone, Copy)]
pub enum DebugMode {
    Enabled,
    Disabled,
}

impl From<DebugMode> for bool {
    fn from(debug_mode: DebugMode) -> Self {
        match debug_mode {
            DebugMode::Enabled => true,
            DebugMode::Disabled => false,
        }
    }
}

impl From<bool> for DebugMode {
    fn from(debug_mode: bool) -> DebugMode {
        if debug_mode {
            DebugMode::Enabled
        } else {
            DebugMode::Disabled
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Client;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn client_is_send_sync() {
        assert_send_sync::<Client<()>>();
    }
}

use alloc::string::String;

#[cfg(any(feature = "std", target_arch = "wasm32"))]
pub use crate::RemoteTransactionProver;

/// Default remote prover endpoint for testnet.
pub const TESTNET_PROVER_ENDPOINT: &str = "https://tx-prover.testnet.miden.io";

/// Default remote prover endpoint for devnet.
pub const DEVNET_PROVER_ENDPOINT: &str = "https://tx-prover.devnet.miden.io";

/// Default timeout in milliseconds for gRPC connections (10 seconds).
pub const DEFAULT_GRPC_TIMEOUT_MS: u64 = 10_000;

/// Configuration for lazy note transport initialization.
///
/// Since `GrpcNoteTransportClient::connect()` is async, this struct allows us to defer
/// the connection until `build()` is called.
pub struct NoteTransportConfig {
    pub endpoint: String,
    pub timeout_ms: u64,
}

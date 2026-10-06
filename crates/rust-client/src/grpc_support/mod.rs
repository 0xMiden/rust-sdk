use alloc::string::String;
#[cfg(feature = "tonic")]
use core::time::Duration;

#[cfg(feature = "tonic")]
pub use crate::RemoteTransactionProver;

/// Default remote prover endpoint for mainnet.
pub const MAINNET_PROVER_ENDPOINT: &str = "https://tx-prover.mainnet.miden.io";

/// Default remote prover endpoint for testnet.
pub const TESTNET_PROVER_ENDPOINT: &str = "https://tx-prover.testnet.miden.io";

/// Default remote prover endpoint for devnet.
pub const DEVNET_PROVER_ENDPOINT: &str = "https://tx-prover.devnet.miden.io";

/// Default timeout in milliseconds for gRPC connections (10 seconds).
pub const DEFAULT_GRPC_TIMEOUT_MS: u64 = 10_000;

/// Configuration for lazy note transport initialization.
///
/// Since `GrpcNoteTransportClient::connect()` is async, this struct allows us to defer the
/// connection until `build()` is called.
pub struct NoteTransportConfig {
    pub endpoint: String,
    pub timeout_ms: u64,
}

// RETRY HELPERS
// ================================================================================================

/// Returns the delay that the `retry-after` metadata value of `status` requests, in whole seconds.
#[cfg(feature = "tonic")]
pub(crate) fn extract_retry_after(status: &tonic::Status) -> Option<Duration> {
    status
        .metadata()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(all(feature = "tonic", not(target_arch = "wasm32")))]
pub(crate) async fn async_sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// On WASM, sleep using browser timers so retry delays are honored.
#[cfg(all(feature = "tonic", target_arch = "wasm32"))]
pub(crate) async fn async_sleep(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

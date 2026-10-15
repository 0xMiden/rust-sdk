//! Integration tests for the miden client, and the harness they drive a client against a running
//! node with.
//!
//! The harness lives in this library rather than in the test binaries so that the CLI's tests and
//! `miden-bench` can build their clients from the same [`ClientConfig`] and fund them through the
//! same [`funding`] service.

pub mod config;
pub mod funding;
pub mod tests;

pub use config::{ClientConfig, NoteTransportEndpoint, create_test_auth_path};
use tracing_subscriber::EnvFilter;

// TRACING
// ================================================================================================

/// Installs a global `tracing` subscriber that writes events to the test output.
///
/// The `RUST_LOG` environment variable sets the filter. The default filter is `info`. The test
/// harness captures the output and shows it for a test that fails.
pub fn init_tracing() {
    // The call fails when an earlier test in the same process installed a subscriber. That
    // subscriber stays in use.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_test_writer()
        .try_init();
}

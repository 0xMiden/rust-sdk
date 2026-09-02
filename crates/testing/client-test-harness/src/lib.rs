//! Shared harness for the tests that drive a real client against a running node.
//!
//! Lives outside the test binaries so that both the integration tests and the CLI's tests can use
//! it without one binary crate depending on another.

pub mod config;
pub mod fee_funding;

pub use config::{ClientConfig, NoteTransportEndpoint, create_test_auth_path};

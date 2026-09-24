//! Integration tests for the miden client, and the harness they drive a client against a running
//! node with.
//!
//! The harness lives in this library rather than in the test binaries so that the CLI's tests and
//! `miden-bench` can build their clients from the same [`ClientConfig`] and fund them through the
//! same [`funding`] service.

pub mod config;
pub mod funding;
pub mod submit_retry;
pub mod tests;

pub use config::{ClientConfig, NoteTransportEndpoint, create_test_auth_path};

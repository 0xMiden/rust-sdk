//! gRPC implementation of the miden-client node RPC client.

#![no_std]

#[macro_use]
extern crate alloc;

#[cfg(feature = "std")]
extern crate std;

mod conversions;
mod grpc_support;
mod node_errors;

#[cfg(not(feature = "testing"))]
mod generated;
#[cfg(feature = "testing")]
pub mod generated;

mod tonic_client;
pub use tonic_client::GrpcClient;

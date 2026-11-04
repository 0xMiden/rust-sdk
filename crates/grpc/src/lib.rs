//! Default gRPC implementation of the `miden-client-core` `NodeRpcClient` trait.

mod conversions;
mod endpoint;
pub use endpoint::Endpoint;
mod grpc_support;
mod node_errors;

mod generated;

mod tonic_client;
pub use tonic_client::GrpcClient;

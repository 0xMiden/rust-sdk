//! Default gRPC implementation of the `miden-client-core` `NodeRpcClient` trait.
//!
//! [`GrpcClient::call`] calls one endpoint of a specific node RPC API version, such as the types in
//! [`v1`].

mod api_version;
mod conversions;
mod endpoint;
pub use endpoint::Endpoint;
mod grpc_support;
mod node_errors;

mod generated;

mod rpc_method;
pub use rpc_method::RpcMethod;

mod tonic_client;
pub use tonic_client::GrpcClient;

mod pagination;
mod validation;

pub mod v1;

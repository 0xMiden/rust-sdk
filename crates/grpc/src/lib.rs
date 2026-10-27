//! Default gRPC implementation of the `miden-client-core` `NodeRpcClient` trait.

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

pub mod v1;

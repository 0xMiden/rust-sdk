//! Default gRPC clients for the services that a Miden client uses.
//!
//! - [`GrpcClient`] implements the `miden-client-core` `NodeRpcClient` trait for a Miden node.
//!   [`GrpcClient::call`] calls one endpoint of a specific node RPC API version, such as the types
//!   in [`v1`]. [`GrpcClient::subscribe`] opens one server-streaming endpoint.
//! - [`note_transport::GrpcNoteTransportClient`] implements the `miden-client-core`
//!   `NoteTransportClient` trait for the note transport service.
//! - [`remote_prover::RemoteTransactionProver`] proves transactions on a remote prover.

mod conversions;
mod endpoint;
pub use endpoint::Endpoint;
mod grpc_support;
mod node_errors;

mod generated;

pub mod note_transport;
pub mod remote_prover;

mod rpc_method;
pub use rpc_method::RpcMethod;

mod rpc_stream_method;
pub use miden_client_core::rpc::RpcStream;
pub use rpc_stream_method::RpcStreamMethod;

mod tonic_client;
pub use tonic_client::GrpcClient;

mod pagination;
mod validation;

pub mod v1;

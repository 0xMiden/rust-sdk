//! Provides an interface for the client to communicate with a Miden node using Remote Procedure
//! Calls (RPC).
//!
//! The [`NodeRpcClient`] trait is defined in `miden-client-core` and is re-exported here.
//!
//! The client implementation adapts to the target environment automatically:
//! - Native targets use `tonic` transport with TLS.
//! - `wasm32` targets use `tonic-web-wasm-client` transport.
//!
//! ## Example
//!
//! ```no_run
//! # use miden_client::rpc::{Endpoint, NodeRpcClient, GrpcClient, VerifyingRpcClient};
//! # use miden_protocol::block::BlockNumber;
//! # #[tokio::main]
//! # async fn main() -> Result<(), Box<dyn std::error::Error>> {
//! // Create a gRPC client instance (assumes default endpoint configuration), wrapped so that
//! // node responses are verified against the requests.
//! let endpoint = Endpoint::new("https".into(), "localhost".into(), Some(57291));
//! let rpc_client = VerifyingRpcClient::new(GrpcClient::new(&endpoint, 1000));
//!
//! // Fetch the latest block header (by passing None).
//! let (block_header, mmr_proof) = rpc_client.get_block_header_by_number(None, true).await?;
//!
//! println!("Latest block number: {}", block_header.block_num());
//! if let Some(proof) = mmr_proof {
//!     println!("MMR proof received accordingly");
//! }
//!
//! #    Ok(())
//! # }
//! ```
//! The client also makes use of this component in order to communicate with the node.
//!
//! For further details and examples, see the documentation for the individual methods in the
//! [`NodeRpcClient`] trait.

pub mod encryption;

pub use domain::status::{NetworkNoteStatus, NetworkNoteStatusInfo, RpcStatusInfo};
pub use miden_client_core::errors::{
    AcceptHeaderContext,
    AcceptHeaderError,
    AddTransactionError,
    ConversionError,
    EndpointError,
    GrpcError,
    RegisterAccountError,
    RpcConversionError,
    RpcError,
    node,
};
pub use miden_client_core::rpc::{
    AccountStateAt,
    NodeRpcClient,
    NoteContentFetch,
    RpcEndpoint,
    RpcLimits,
    VerifyingRpcClient,
    domain,
};
#[cfg(feature = "tonic")]
pub use miden_client_grpc::{Endpoint, GrpcClient};

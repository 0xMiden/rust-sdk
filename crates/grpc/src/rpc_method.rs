use miden_client_core::errors::RpcError;
use miden_protocol::vm::FutureMaybeSend;

use crate::GrpcClient;

/// A call to one node RPC endpoint in one API version.
///
/// [`GrpcClient::call`] runs [`execute`](Self::execute) and then [`validate`](Self::validate).
pub trait RpcMethod {
    /// The decoded response.
    type Output;

    /// Sends the request to the node and decodes the response.
    fn execute(&self, client: &GrpcClient) -> impl FutureMaybeSend<Result<Self::Output, RpcError>>;

    /// Checks the decoded response against the request.
    fn validate(&self, output: &Self::Output) -> Result<(), RpcError>;
}

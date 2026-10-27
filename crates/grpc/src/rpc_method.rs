use miden_client_core::errors::RpcError;

use crate::GrpcClient;

/// A call to one node RPC endpoint in one API version.
///
/// [`GrpcClient::call`] runs [`execute`](Self::execute) and then [`validate`](Self::validate).
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait RpcMethod {
    /// The decoded response.
    type Output;

    /// Sends the request to the node and decodes the response.
    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError>;

    /// Checks the decoded response against the request.
    fn validate(&self, output: &Self::Output) -> Result<(), RpcError>;
}

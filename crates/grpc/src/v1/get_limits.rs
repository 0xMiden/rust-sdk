use miden_client_core::errors::RpcError;
use miden_client_core::rpc::{RpcEndpoint, RpcLimits};

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the request limits of the node.
pub struct GetLimits;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetLimits {
    type Output = RpcLimits;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let response = client
            .call_with_retry(RpcEndpoint::GetLimits, |mut rpc_api| {
                Box::pin(
                    async move { rpc_api.get_limits(proto::node::v1::GetLimitsRequest {}).await },
                )
            })
            .await?;

        RpcLimits::try_from(response.into_inner()).map_err(RpcError::from)
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::{RpcEndpoint, RpcLimits};

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the request limits of the node.
pub struct GetLimits;

impl RpcMethod for GetLimits {
    type Output = RpcLimits;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let response = client
            .send_with_retry(RpcEndpoint::GetLimits, |mut rpc_api| async move {
                rpc_api.get_limits(proto::node::v1::GetLimitsRequest {}).await
            })
            .await?;

        RpcLimits::try_from(response.into_inner()).map_err(RpcError::from)
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

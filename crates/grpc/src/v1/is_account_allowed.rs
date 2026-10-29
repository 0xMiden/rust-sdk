use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_protocol::account::AccountId;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Checks whether the node allows an account.
pub struct IsAccountAllowed {
    /// The account to check.
    pub account_id: AccountId,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for IsAccountAllowed {
    type Output = bool;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request =
            proto::node::v1::IsAccountAllowedRequest { account_id: Some(self.account_id.into()) };

        let response = client
            .call_with_retry(RpcEndpoint::IsAccountAllowed, |mut rpc_api| {
                Box::pin(async move { rpc_api.is_account_allowed(request).await })
            })
            .await?;

        Ok(response.into_inner().allowed)
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

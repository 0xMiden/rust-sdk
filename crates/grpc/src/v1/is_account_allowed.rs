use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_protocol::account::AccountId;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Checks whether the node allows an account.
pub struct IsAccountAllowed {
    /// The account to check.
    pub account_id: AccountId,
}

impl RpcMethod for IsAccountAllowed {
    type Output = bool;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request =
            proto::node::v1::IsAccountAllowedRequest { account_id: Some(self.account_id.into()) };

        let response = client
            .send_with_retry(RpcEndpoint::IsAccountAllowed, |mut rpc_api| async move {
                rpc_api.is_account_allowed(request).await
            })
            .await?;

        Ok(response.into_inner().allowed)
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

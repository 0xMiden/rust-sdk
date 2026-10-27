use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_protocol::account::AccountId;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Registers an account with the node, using an invitation code.
pub struct RegisterAccount<'a> {
    /// The invitation code. It is a secret.
    pub invitation_code: &'a str,
    /// The account to register.
    pub account_id: AccountId,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for RegisterAccount<'_> {
    type Output = ();

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        // The invitation code is a secret. Keep it out of logs and out of error messages.
        let request = proto::node::v1::RegisterAccountRequest {
            invitation_code: self.invitation_code.to_string(),
            account_id: Some(self.account_id.into()),
        };

        client
            .call_with_retry(RpcEndpoint::RegisterAccount, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.register_account(request).await })
            })
            .await?;

        Ok(())
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        // A successful response carries no payload to check the request against.
        Ok(())
    }
}

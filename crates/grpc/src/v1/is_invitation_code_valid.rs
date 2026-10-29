use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Checks whether the node accepts an invitation code.
pub struct IsInvitationCodeValid<'a> {
    /// The invitation code. It is a secret.
    pub invitation_code: &'a str,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for IsInvitationCodeValid<'_> {
    type Output = bool;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request = proto::node::v1::IsInvitationCodeValidRequest {
            invitation_code: self.invitation_code.into(),
        };

        let response = client
            .call_with_retry(RpcEndpoint::IsInvitationCodeValid, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.is_invitation_code_valid(request).await })
            })
            .await?;

        Ok(response.into_inner().valid)
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

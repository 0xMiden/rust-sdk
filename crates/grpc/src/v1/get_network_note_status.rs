use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_client_core::rpc::domain::status::NetworkNoteStatusInfo;
use miden_protocol::note::NoteId;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the processing status of a network note.
pub struct GetNetworkNoteStatus {
    /// The note to check.
    pub note_id: NoteId,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetNetworkNoteStatus {
    type Output = NetworkNoteStatusInfo;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request = proto::node::v1::GetNetworkNoteStatusRequest {
            note_id: Some(proto::note::NoteId::from(&self.note_id)),
        };

        let response = client
            .call_with_retry(RpcEndpoint::GetNetworkNoteStatus, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.get_network_note_status(request).await })
            })
            .await?;

        response.into_inner().try_into()
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

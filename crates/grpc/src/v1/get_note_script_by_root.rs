use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_objects::DecodeMessageExt;
use miden_protocol::Word;
use miden_protocol::note::NoteScript;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches a note script by its root.
pub struct GetNoteScriptByRoot {
    /// The root of the script.
    pub root: Word,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetNoteScriptByRoot {
    /// The script, or `None` if the node has no script registered for the root.
    type Output = Option<NoteScript>;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request = proto::node::v1::GetNoteScriptByRootRequest { root: Some(self.root.into()) };

        let response = client
            .call_with_retry(RpcEndpoint::GetNoteScriptByRoot, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.get_note_script_by_root(request).await })
            })
            .await?;

        // The node returns an empty payload when it has no script registered for the root.
        let Some(script) = response.into_inner().script else {
            return Ok(None);
        };
        let note_script: NoteScript = script.decode_and_verify()?;

        Ok(Some(note_script))
    }

    fn validate(&self, output: &Self::Output) -> Result<(), RpcError> {
        if let Some(script) = output {
            let fetched_root = script.root();
            if Word::from(fetched_root) != self.root {
                return Err(RpcError::InvalidResponse(format!(
                    "node returned note script with root {fetched_root} for requested root {}",
                    self.root
                )));
            }
        }
        Ok(())
    }
}

use std::collections::BTreeSet;

use miden_client_core::errors::{RpcConversionError, RpcError};
use miden_client_core::rpc::domain::note::FetchedNote;
use miden_client_core::rpc::{NodeRpcClient, RpcEndpoint};
use miden_protocol::note::NoteId;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches notes by ID.
///
/// The IDs are sent in chunks of at most [`RpcLimits::note_ids_limit`] per request.
///
/// [`RpcLimits::note_ids_limit`]: miden_client_core::rpc::RpcLimits::note_ids_limit
pub struct GetNotesById<'a> {
    /// The IDs of the notes to fetch.
    pub note_ids: &'a [NoteId],
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetNotesById<'_> {
    type Output = Vec<FetchedNote>;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let limits = client.get_rpc_limits().await?;
        let mut notes = Vec::with_capacity(self.note_ids.len());
        for chunk in self.note_ids.chunks(limits.note_ids_limit as usize) {
            let request = proto::node::v1::GetNotesByIdRequest {
                note_ids: chunk.iter().map(proto::note::NoteId::from).collect(),
            };

            let response = client
                .call_with_retry(RpcEndpoint::GetNotesById, |mut rpc_api| {
                    let request = request.clone();
                    Box::pin(async move { rpc_api.get_notes_by_id(request).await })
                })
                .await?;

            let response_notes = response
                .into_inner()
                .notes
                .into_iter()
                .map(FetchedNote::try_from)
                .collect::<Result<Vec<FetchedNote>, RpcConversionError>>()?;

            notes.extend(response_notes);
        }
        Ok(notes)
    }

    fn validate(&self, output: &Self::Output) -> Result<(), RpcError> {
        let requested: BTreeSet<NoteId> = self.note_ids.iter().copied().collect();
        for id in output.iter().map(FetchedNote::id) {
            if !requested.contains(&id) {
                let list = requested.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                return Err(RpcError::InvalidResponse(format!(
                    "node returned note {id} but [{list}] were requested"
                )));
            }
        }
        Ok(())
    }
}

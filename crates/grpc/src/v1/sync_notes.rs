use std::collections::{BTreeMap, BTreeSet};

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::note::{CommittedNote, SyncNotesBlock};
use miden_client_core::rpc::{NodeRpcClient, RpcEndpoint};
use miden_protocol::block::BlockNumber;
use miden_protocol::note::NoteTag;

use crate::generated::node::v1::BlockRange;
use crate::pagination::{BlockPagination, PaginationResult};
use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the committed notes that match a set of tags in a block range.
///
/// The tags are sent in chunks of at most [`RpcLimits::note_tags_limit`] per request, and each
/// chunk is paginated across the block range.
///
/// [`RpcLimits::note_tags_limit`]: miden_client_core::rpc::RpcLimits::note_tags_limit
pub struct SyncNotes<'a> {
    /// The first block of the range.
    pub block_from: BlockNumber,
    /// The last block of the range.
    pub block_to: BlockNumber,
    /// The tags of the notes to fetch.
    pub note_tags: &'a BTreeSet<NoteTag>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for SyncNotes<'_> {
    type Output = Vec<SyncNotesBlock>;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        if self.note_tags.is_empty() {
            return Ok(Vec::new());
        }

        let limits = client.get_rpc_limits().await?;
        let tags: Vec<NoteTag> = self.note_tags.iter().copied().collect();

        // Merge blocks across tag-chunks: a single block can hold notes whose tags fall into
        // different chunks, so the same block can appear in multiple chunks' responses.
        let mut merged_blocks: BTreeMap<BlockNumber, SyncNotesBlock> = BTreeMap::new();

        for chunk in tags.chunks(limits.note_tags_limit as usize) {
            let proto_tags: Vec<u32> = chunk.iter().map(|&t| t.into()).collect();
            let mut pagination = BlockPagination::new(self.block_from, self.block_to);

            loop {
                let request = proto::node::v1::SyncNotesRequest {
                    block_range: Some(BlockRange {
                        block_from: pagination.current_block_from().as_u32(),
                        block_to: self.block_to.as_u32(),
                    }),
                    note_tags: proto_tags.clone(),
                };

                let response = client
                    .call_with_retry(RpcEndpoint::SyncNotes, |mut rpc_api| {
                        let request = request.clone();
                        Box::pin(async move { rpc_api.sync_notes(request).await })
                    })
                    .await?
                    .into_inner();

                let page = response.pagination_info.ok_or(RpcError::ExpectedDataMissing(
                    "SyncNotesResponse.pagination_info".to_owned(),
                ))?;
                let page_chain_tip = BlockNumber::from(page.chain_tip);
                let page_block_to = BlockNumber::from(page.block_num);

                for proto_block in response.blocks {
                    let block: SyncNotesBlock = proto_block.try_into()?;
                    let bn = block.block_header.block_num();
                    if let Some(existing) = merged_blocks.get_mut(&bn) {
                        for (id, note) in block.notes {
                            existing.notes.entry(id).or_insert(note);
                        }
                    } else {
                        merged_blocks.insert(bn, block);
                    }
                }

                match pagination.advance(page_block_to, page_chain_tip)? {
                    PaginationResult::Continue => {},
                    PaginationResult::Done { .. } => break,
                }
            }
        }

        Ok(merged_blocks.into_values().collect())
    }

    fn validate(&self, output: &Self::Output) -> Result<(), RpcError> {
        let returned = output.iter().flat_map(|block| block.notes.values().map(CommittedNote::tag));
        for tag in returned {
            if !self.note_tags.contains(&tag) {
                let list =
                    self.note_tags.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                return Err(RpcError::InvalidResponse(format!(
                    "node returned note with tag {tag} but [{list}] were requested"
                )));
            }
        }
        Ok(())
    }
}

use std::collections::BTreeSet;

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::nullifier::NullifierUpdate;
use miden_client_core::rpc::{NodeRpcClient, RpcEndpoint};
use miden_protocol::block::BlockNumber;

use crate::generated::node::v1::BlockRange;
use crate::pagination::{BlockPagination, PaginationResult};
use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the nullifiers that match a set of 16-bit prefixes in a block range.
///
/// The prefixes are sent in chunks of at most [`RpcLimits::nullifiers_limit`] per request, and each
/// chunk is paginated across the block range.
///
/// [`RpcLimits::nullifiers_limit`]: miden_client_core::rpc::RpcLimits::nullifiers_limit
pub struct SyncNullifiers<'a> {
    /// The 16-bit prefixes of the nullifiers to fetch.
    pub prefixes: &'a [u16],
    /// The first block of the range.
    pub block_from: BlockNumber,
    /// The last block of the range.
    pub block_to: BlockNumber,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for SyncNullifiers<'_> {
    type Output = Vec<NullifierUpdate>;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let limits = client.get_rpc_limits().await?;
        let mut all_nullifiers = BTreeSet::new();

        // If the prefixes are too many, we need to chunk them into smaller groups to avoid
        // violating the RPC limit.
        for chunk in self.prefixes.chunks(limits.nullifiers_limit as usize) {
            let proto_prefixes: Vec<u32> = chunk.iter().map(|&x| u32::from(x)).collect();
            let mut pagination = BlockPagination::new(self.block_from, self.block_to);

            loop {
                let request = proto::node::v1::SyncNullifiersRequest {
                    nullifiers: proto_prefixes.clone(),
                    prefix_len: 16,
                    block_range: Some(BlockRange {
                        block_from: pagination.current_block_from().as_u32(),
                        block_to: pagination.block_to().as_u32(),
                    }),
                };

                let response = client
                    .call_with_retry(RpcEndpoint::SyncNullifiers, |mut rpc_api| {
                        let request = request.clone();
                        Box::pin(async move { rpc_api.sync_nullifiers(request).await })
                    })
                    .await?
                    .into_inner();

                let batch_nullifiers = response
                    .nullifiers
                    .iter()
                    .map(TryFrom::try_from)
                    .collect::<Result<Vec<NullifierUpdate>, _>>()
                    .map_err(|err| RpcError::InvalidResponse(err.to_string()))?;

                all_nullifiers.extend(batch_nullifiers);

                let page = response.pagination_info.ok_or(RpcError::ExpectedDataMissing(
                    "SyncNullifiersResponse.pagination_info".to_owned(),
                ))?;

                match pagination.advance(page.block_num.into(), page.chain_tip.into())? {
                    PaginationResult::Continue => {},
                    PaginationResult::Done { .. } => break,
                }
            }
        }
        Ok(all_nullifiers.into_iter().collect::<Vec<_>>())
    }

    fn validate(&self, output: &Self::Output) -> Result<(), RpcError> {
        let requested_prefixes: BTreeSet<u16> = self.prefixes.iter().copied().collect();
        for update in output {
            let prefix = update.nullifier.prefix();
            if !requested_prefixes.contains(&prefix) {
                let requested = requested_prefixes
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(RpcError::InvalidResponse(format!(
                    "node returned nullifier with prefix {prefix} but [{requested}] were requested"
                )));
            }
            if update.block_num < self.block_from || update.block_num > self.block_to {
                return Err(RpcError::InvalidResponse(format!(
                    "node returned nullifier {} at block {} but blocks {} to {} were requested",
                    update.nullifier.to_hex(),
                    update.block_num,
                    self.block_from,
                    self.block_to,
                )));
            }
        }
        Ok(())
    }
}

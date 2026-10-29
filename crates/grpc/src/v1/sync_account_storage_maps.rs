use std::collections::BTreeMap;

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_client_core::rpc::domain::storage_map::StorageMapInfo;
use miden_protocol::account::{AccountId, StorageMapPatchEntries, StorageSlotName};
use miden_protocol::block::BlockNumber;

use crate::generated::node::v1::BlockRange;
use crate::pagination::{BlockPagination, PaginationResult};
use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the storage map updates of an account in a block range, paginated across the range.
pub struct SyncAccountStorageMaps {
    /// The first block of the range.
    pub block_from: BlockNumber,
    /// The last block of the range.
    pub block_to: BlockNumber,
    /// The account to fetch.
    pub account_id: AccountId,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for SyncAccountStorageMaps {
    type Output = StorageMapInfo;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let mut pagination = BlockPagination::new(self.block_from, self.block_to);
        let mut map_entries: BTreeMap<StorageSlotName, StorageMapPatchEntries> = BTreeMap::new();

        let (chain_tip, block_number) = loop {
            let request = proto::node::v1::SyncAccountStorageMapsRequest {
                block_range: Some(BlockRange {
                    block_from: pagination.current_block_from().as_u32(),
                    block_to: self.block_to.as_u32(),
                }),
                account_id: Some(self.account_id.into()),
            };
            let response = client
                .call_with_retry(RpcEndpoint::SyncStorageMaps, |mut rpc_api| {
                    Box::pin(async move { rpc_api.sync_account_storage_maps(request).await })
                })
                .await?;
            let page = StorageMapInfo::try_from(response.into_inner())?;

            for (slot_name, entries) in page.map_entries {
                map_entries
                    .entry(slot_name)
                    .or_default()
                    .as_map_mut()
                    .extend(entries.into_map());
            }

            match pagination.advance(page.block_number, page.chain_tip)? {
                PaginationResult::Continue => {},
                PaginationResult::Done {
                    chain_tip: final_chain_tip,
                    block_num: final_block_num,
                } => break (final_chain_tip, final_block_num),
            }
        };

        Ok(StorageMapInfo { chain_tip, block_number, map_entries })
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

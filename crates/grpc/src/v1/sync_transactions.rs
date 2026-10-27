use std::collections::BTreeSet;

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::transaction::TransactionRecord;
use miden_client_core::rpc::{NodeRpcClient, RpcEndpoint};
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;

use crate::generated::node::v1::BlockRange;
use crate::pagination::{BlockPagination, PaginationResult};
use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the transaction records of a set of accounts in a block range.
///
/// The account IDs are sent in chunks of at most [`RpcLimits::account_ids_limit`] per request, and
/// each chunk is paginated across the block range.
///
/// [`RpcLimits::account_ids_limit`]: miden_client_core::rpc::RpcLimits::account_ids_limit
pub struct SyncTransactions {
    /// The first block of the range.
    pub block_from: BlockNumber,
    /// The last block of the range.
    pub block_to: BlockNumber,
    /// The accounts to fetch.
    pub account_ids: Vec<AccountId>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for SyncTransactions {
    type Output = Vec<TransactionRecord>;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        if self.account_ids.is_empty() {
            return Ok(Vec::new());
        }

        let limits = client.get_rpc_limits().await?;
        let mut transactions: Vec<TransactionRecord> = Vec::new();

        for chunk in self.account_ids.chunks(limits.account_ids_limit as usize) {
            let proto_account_ids: Vec<_> = chunk.iter().map(|acc_id| (*acc_id).into()).collect();
            let mut pagination = BlockPagination::new(self.block_from, self.block_to);

            loop {
                let request = proto::node::v1::SyncTransactionsRequest {
                    block_range: Some(BlockRange {
                        block_from: pagination.current_block_from().as_u32(),
                        block_to: self.block_to.as_u32(),
                    }),
                    account_ids: proto_account_ids.clone(),
                };

                let response = client
                    .call_with_retry(RpcEndpoint::SyncTransactions, |mut rpc_api| {
                        let request = request.clone();
                        Box::pin(async move { rpc_api.sync_transactions(request).await })
                    })
                    .await?
                    .into_inner();

                let page = response.pagination_info.ok_or(RpcError::ExpectedDataMissing(
                    "SyncTransactionsResponse.pagination_info".to_owned(),
                ))?;
                let page_chain_tip = BlockNumber::from(page.chain_tip);
                let page_block_to = BlockNumber::from(page.block_num);

                for proto_tx in response.transactions {
                    transactions.push(TransactionRecord::try_from(proto_tx)?);
                }

                match pagination.advance(page_block_to, page_chain_tip)? {
                    PaginationResult::Continue => {},
                    PaginationResult::Done { .. } => break,
                }
            }
        }

        Ok(transactions)
    }

    fn validate(&self, output: &Self::Output) -> Result<(), RpcError> {
        let requested: BTreeSet<AccountId> = self.account_ids.iter().copied().collect();
        for record in output {
            let id = record.transaction_header.account_id();
            if !requested.contains(&id) {
                let list = requested.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
                return Err(RpcError::InvalidResponse(format!(
                    "node returned transaction for account {id} but [{list}] were requested"
                )));
            }
        }
        Ok(())
    }
}

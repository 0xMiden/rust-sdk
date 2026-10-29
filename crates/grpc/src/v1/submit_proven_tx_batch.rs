use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_client_core::rpc::encryption::SealedTransactionInputs;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::BlockNumber;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Submits a proven transaction batch to the node.
pub struct SubmitProvenTxBatch<'a> {
    /// The proven batch.
    pub proven_batch: &'a ProvenBatch,
    /// The batch that was proven.
    pub proposed_batch: &'a ProposedBatch,
    /// The inputs of each transaction in the batch, sealed for the node.
    pub sealed_transaction_inputs: Vec<SealedTransactionInputs>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for SubmitProvenTxBatch<'_> {
    /// The block number of the node's chain tip when it accepts the batch.
    type Output = BlockNumber;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let submission = proto::submission::TransactionBatch {
            batch: Some(self.proven_batch.into()),
            proposed_batch: Some(self.proposed_batch.into()),
            sealed_transaction_inputs: self
                .sealed_transaction_inputs
                .iter()
                .cloned()
                .map(Into::into)
                .collect(),
        };

        let request = proto::node::v1::SubmitProvenTxBatchRequest { submission: Some(submission) };
        let response = client
            .call_with_retry(RpcEndpoint::SubmitProvenBatch, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.submit_proven_tx_batch(request).await })
            })
            .await?;

        Ok(BlockNumber::from(response.into_inner().block_num))
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

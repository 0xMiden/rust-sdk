use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_client_core::rpc::encryption::SealedTransactionInputs;
use miden_protocol::block::BlockNumber;
use miden_protocol::transaction::ProvenTransaction;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Submits a proven transaction to the node.
pub struct SubmitProvenTx<'a> {
    /// The proven transaction.
    pub transaction: &'a ProvenTransaction,
    /// The transaction inputs, sealed for the node.
    pub sealed_transaction_inputs: SealedTransactionInputs,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for SubmitProvenTx<'_> {
    /// The block number of the node's chain tip when it accepts the transaction.
    type Output = BlockNumber;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let submission = proto::submission::ProvenTransactionSubmission {
            transaction: Some(self.transaction.into()),
            sealed_transaction_inputs: Some(self.sealed_transaction_inputs.clone().into()),
        };

        let request = proto::node::v1::SubmitProvenTxRequest { submission: Some(submission) };
        let response = client
            .call_with_retry(RpcEndpoint::SubmitProvenTx, |mut rpc_api| {
                let request = request.clone();
                Box::pin(async move { rpc_api.submit_proven_tx(request).await })
            })
            .await?;

        Ok(BlockNumber::from(response.into_inner().block_num))
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

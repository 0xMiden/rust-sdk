use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_client_core::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
use miden_protocol::block::BlockNumber;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the chain MMR updates from the client's block height up to the target.
pub struct SyncChainMmr {
    /// The block height of the client.
    pub current_block_height: BlockNumber,
    /// The block to sync up to.
    pub upper_bound: SyncTarget,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for SyncChainMmr {
    type Output = ChainMmrInfo;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let finality_level: proto::node::v1::FinalityLevel = self.upper_bound.into();

        let request = proto::node::v1::SyncChainMmrRequest {
            current_client_block_height: self.current_block_height.as_u32(),
            finality_level: finality_level.into(),
        };

        let response = client
            .call_with_retry(RpcEndpoint::SyncChainMmr, |mut rpc_api| {
                Box::pin(async move { rpc_api.sync_chain_mmr(request).await })
            })
            .await?;

        response.into_inner().try_into()
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        Ok(())
    }
}

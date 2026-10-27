use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_objects::DecodeMessageExt;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::crypto::merkle::mmr::{Forest, MmrPath, MmrProof};
use miden_protocol::protocol_config::ProtocolConfig;
use tracing::info;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches a block header.
pub struct GetBlockHeaderByNumber {
    /// The block to fetch. `None` fetches the latest block.
    pub block_num: Option<BlockNumber>,
    /// Requests the MMR proof of the block against the current chain.
    pub include_mmr_proof: bool,
    /// Requests the protocol configuration that the block header commits to.
    pub include_protocol_config: bool,
}

/// The response to [`GetBlockHeaderByNumber`].
pub struct BlockHeaderResponse {
    /// The requested block header.
    pub block_header: BlockHeader,
    /// The MMR proof of the block. It is present only if the request includes it.
    pub mmr_proof: Option<MmrProof>,
    /// The protocol configuration of the block. It is present only if the request includes it.
    pub protocol_config: Option<ProtocolConfig>,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetBlockHeaderByNumber {
    type Output = BlockHeaderResponse;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request = proto::node::v1::GetBlockHeaderByNumberRequest {
            block_num: self.block_num.as_ref().map(BlockNumber::as_u32),
            include_mmr_proof: Some(self.include_mmr_proof),
            include_protocol_config: Some(self.include_protocol_config),
        };

        info!("Calling GetBlockHeaderByNumber: {:?}", request);

        let response = client
            .call_with_retry(RpcEndpoint::GetBlockHeaderByNumber, |mut rpc_api| {
                Box::pin(async move { rpc_api.get_block_header_by_number(request).await })
            })
            .await?
            .into_inner();

        let block_header: BlockHeader = response
            .block_header
            .ok_or(RpcError::ExpectedDataMissing("BlockHeader".into()))?
            .decode_and_build_unchecked()?;

        let mmr_proof = if self.include_mmr_proof {
            let forest = response
                .chain_length
                .ok_or(RpcError::ExpectedDataMissing("ChainLength".into()))?;
            let merkle_path: MerklePath = response
                .mmr_path
                .ok_or(RpcError::ExpectedDataMissing("MmrPath".into()))?
                .decode_and_verify()?;

            let forest_size = usize::try_from(forest).expect("u64 should fit in usize");
            let forest = Forest::new(forest_size).map_err(|_| {
                RpcError::InvalidResponse(format!("invalid forest size: {forest_size}"))
            })?;
            Some(MmrProof::new(
                MmrPath::new(forest, block_header.block_num().as_usize(), merkle_path),
                block_header.commitment(),
            ))
        } else {
            None
        };

        let protocol_config = if self.include_protocol_config {
            let protocol_config: ProtocolConfig = response
                .protocol_config
                .ok_or(RpcError::ExpectedDataMissing("ProtocolConfig".into()))?
                .decode_and_verify()?;
            Some(protocol_config)
        } else {
            None
        };

        Ok(BlockHeaderResponse { block_header, mmr_proof, protocol_config })
    }

    fn validate(&self, output: &Self::Output) -> Result<(), RpcError> {
        let returned = output.block_header.block_num();
        if let Some(requested) = self.block_num
            && returned != requested
        {
            return Err(RpcError::InvalidResponse(format!(
                "node returned block {returned} but block {requested} was requested"
            )));
        }

        // The store keys a configuration by its own commitment, so a configuration that does not
        // match the header is unusable.
        if let Some(protocol_config) = &output.protocol_config {
            let returned = protocol_config.to_commitment();
            let expected = output.block_header.protocol_config_commitment();
            if returned != expected {
                return Err(RpcError::InvalidResponse(format!(
                    "node returned a protocol configuration with commitment {returned} for a block header that commits to {expected}"
                )));
            }
        }

        Ok(())
    }
}

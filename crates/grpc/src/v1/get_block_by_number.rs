use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_objects::DecodeMessageExt;
use miden_protocol::block::{BlockNumber, SignedBlock};
use miden_protocol::vm::ExecutionProof;

use crate::validation::verify_block_num;
use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches a signed block and, optionally, its proof.
pub struct GetBlockByNumber {
    /// The block to fetch.
    pub block_num: BlockNumber,
    /// Requests the proof of the block.
    pub include_proof: bool,
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetBlockByNumber {
    /// The block, and its proof if it was requested and the block is proven.
    type Output = (SignedBlock, Option<ExecutionProof>);

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let request = proto::node::v1::GetBlockByNumberRequest {
            block_num: self.block_num.as_u32(),
            include_proof: Some(self.include_proof),
        };

        let response = client
            .call_with_retry(RpcEndpoint::GetBlockByNumber, |mut rpc_api| {
                Box::pin(async move { rpc_api.get_block_by_number(request).await })
            })
            .await?;

        decode_block_response(response.into_inner())
    }

    fn validate(&self, (block, _proof): &Self::Output) -> Result<(), RpcError> {
        verify_block_num(Some(self.block_num), block.header().block_num())
    }
}

/// Decodes the response of `GetBlockByNumber`.
///
/// The response carries the signed block and its proof in separate fields, so the block bytes
/// decode as a [`SignedBlock`] and never as a `ProvenBlock`. The node omits the proof when it is
/// not requested, and also when the block is not proven yet, so an absent proof is not an error.
fn decode_block_response(
    response: proto::node::v1::GetBlockByNumberResponse,
) -> Result<(SignedBlock, Option<ExecutionProof>), RpcError> {
    let block: SignedBlock = response
        .block
        .ok_or(RpcError::ExpectedDataMissing("GetBlockByNumberResponse.block".to_string()))?
        .decode_and_build_unchecked()?;

    let proof = response.proof.map(ExecutionProof::try_from).transpose()?;

    Ok((block, proof))
}

#[cfg(test)]
mod tests {
    use miden_client_core::errors::RpcError;
    use miden_protocol::block::SignedBlock;
    use miden_testing::MockChain;

    use super::{decode_block_response, proto};

    /// Returns the signed block and proof messages of the mock chain's genesis block.
    fn genesis_block_messages()
    -> (proto::blockchain::SignedBlock, proto::primitives::ExecutionProof) {
        let chain = MockChain::new();
        let block = chain.proven_blocks().first().expect("the chain has a genesis block").clone();
        let (header, body, signatures, proof) = block.into_parts();

        (SignedBlock::new_unchecked(header, body, signatures).into(), proof.into())
    }

    #[test]
    fn decode_block_response_reads_a_requested_proof() {
        let (block, proof_message) = genesis_block_messages();
        let response = proto::node::v1::GetBlockByNumberResponse {
            block: Some(block),
            proof: Some(proof_message.clone()),
        };

        let (_block, proof) = decode_block_response(response).unwrap();

        let decoded: proto::primitives::ExecutionProof =
            proof.expect("the response carries a proof").into();
        assert_eq!(decoded, proof_message);
    }

    #[test]
    fn decode_block_response_omits_an_absent_proof() {
        let (block, _) = genesis_block_messages();
        let response =
            proto::node::v1::GetBlockByNumberResponse { block: Some(block), proof: None };

        let (_block, proof) = decode_block_response(response).unwrap();

        assert!(proof.is_none());
    }

    #[test]
    fn decode_block_response_rejects_malformed_proof_bytes() {
        let (block, _) = genesis_block_messages();
        let response = proto::node::v1::GetBlockByNumberResponse {
            block: Some(block),
            proof: Some(proto::primitives::ExecutionProof { encoded: vec![0xff; 32] }),
        };

        let res = decode_block_response(response);

        assert!(matches!(res, Err(RpcError::DeserializationError(_))));
    }

    #[test]
    fn decode_block_response_rejects_an_absent_block() {
        let response = proto::node::v1::GetBlockByNumberResponse { block: None, proof: None };

        let res = decode_block_response(response);

        assert!(matches!(res, Err(RpcError::ExpectedDataMissing(_))));
    }
}

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::block::SubscribedProof;
use miden_client_core::rpc::{RpcEndpoint, RpcStream};
use miden_protocol::block::BlockNumber;
use miden_protocol::vm::ExecutionProof;

use crate::{GrpcClient, RpcStreamMethod, generated as proto};

/// Subscribes to the block proofs of the node.
///
/// The node first sends the proofs that are already available, and then sends each new proof when
/// it proves a block. The genesis block has no proof. Subscribing from [`BlockNumber::GENESIS`]
/// ends the stream with an error.
pub struct ProofSubscription {
    /// The first block of the stream.
    block_from: BlockNumber,
    /// The block of the last validated proof. It is `None` before the first item.
    previous_block_num: Option<BlockNumber>,
}

impl ProofSubscription {
    /// Returns a subscription that starts at `block_from`.
    pub fn new(block_from: BlockNumber) -> Self {
        Self { block_from, previous_block_num: None }
    }
}

impl RpcStreamMethod for ProofSubscription {
    type Item = SubscribedProof;

    async fn open(&self, client: &GrpcClient) -> Result<RpcStream<Self::Item>, RpcError> {
        let request =
            proto::node::v1::ProofSubscriptionRequest { block_from: self.block_from.as_u32() };

        client
            .open_stream(
                RpcEndpoint::ProofSubscription,
                move |mut rpc_api| async move { rpc_api.proof_subscription(request).await },
                decode_subscribed_proof,
            )
            .await
    }

    fn validate(&mut self, item: &Self::Item) -> Result<(), RpcError> {
        let next_block_num = self.previous_block_num.map_or(self.block_from, BlockNumber::child);

        if item.block_num != next_block_num {
            return Err(RpcError::InvalidResponse(format!(
                "node sent proof for block {} but block {next_block_num} is the next block of the subscription",
                item.block_num
            )));
        }

        if item.proven_chain_tip < item.block_num {
            return Err(RpcError::InvalidResponse(format!(
                "node sent proof for block {} with the proven chain tip {}",
                item.block_num, item.proven_chain_tip
            )));
        }

        self.previous_block_num = Some(item.block_num);

        Ok(())
    }
}

/// Decodes one item of the `ProofSubscription` stream.
fn decode_subscribed_proof(
    response: proto::node::v1::ProofSubscriptionResponse,
) -> Result<SubscribedProof, RpcError> {
    let proof =
        ExecutionProof::try_from(response.proof.ok_or(RpcError::ExpectedDataMissing(
            "ProofSubscriptionResponse.proof".to_string(),
        ))?)?;

    Ok(SubscribedProof {
        block_num: BlockNumber::from(response.block_num),
        proof,
        proven_chain_tip: BlockNumber::from(response.proven_chain_tip),
    })
}

#[cfg(test)]
mod tests {
    use miden_client_core::errors::RpcError;
    use miden_protocol::block::BlockNumber;
    use miden_testing::MockChain;

    use super::{decode_subscribed_proof, proto};

    fn genesis_proof_message() -> proto::primitives::ExecutionProof {
        let chain = MockChain::new();
        let block = chain.proven_blocks().first().expect("the chain has a genesis block").clone();
        let (_, _, _, proof) = block.into_parts();

        proof.into()
    }

    #[test]
    fn decode_subscribed_proof_reads_a_valid_response() {
        let proof_message = genesis_proof_message();
        let response = proto::node::v1::ProofSubscriptionResponse {
            block_num: 1,
            proof: Some(proof_message.clone()),
            proven_chain_tip: 3,
        };

        let item = decode_subscribed_proof(response).unwrap();

        assert_eq!(item.block_num, BlockNumber::from(1u32));
        assert_eq!(item.proven_chain_tip, BlockNumber::from(3u32));
        assert_eq!(proto::primitives::ExecutionProof::from(item.proof), proof_message);
    }

    #[test]
    fn decode_subscribed_proof_rejects_an_absent_proof() {
        let response = proto::node::v1::ProofSubscriptionResponse {
            block_num: 1,
            proof: None,
            proven_chain_tip: 1,
        };

        let res = decode_subscribed_proof(response);

        assert!(matches!(res, Err(RpcError::ExpectedDataMissing(_))));
    }

    #[test]
    fn decode_subscribed_proof_rejects_malformed_proof_bytes() {
        let response = proto::node::v1::ProofSubscriptionResponse {
            block_num: 1,
            proof: Some(proto::primitives::ExecutionProof { encoded: vec![0xff; 32] }),
            proven_chain_tip: 1,
        };

        let res = decode_subscribed_proof(response);

        assert!(matches!(res, Err(RpcError::DeserializationError(_))));
    }
}

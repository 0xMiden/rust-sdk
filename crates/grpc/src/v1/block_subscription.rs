use miden_client_core::errors::RpcError;
use miden_client_core::rpc::domain::block::SubscribedBlock;
use miden_client_core::rpc::{RpcEndpoint, RpcStream};
use miden_objects::DecodeMessageExt;
use miden_protocol::Word;
use miden_protocol::block::{BlockNumber, SignedBlock};
use miden_protocol::protocol_config::ProtocolConfig;

use crate::{GrpcClient, RpcStreamMethod, generated as proto};

/// Subscribes to the committed blocks of the node.
///
/// The node first sends the blocks that are already committed, and then sends each new block when
/// it commits the block.
///
/// A block that exceeds the response size limit of the client ends the stream with an error. See
/// [`GrpcClient::with_max_decoding_message_size`].
pub struct BlockSubscription {
    /// The first block of the stream.
    pub block_from: BlockNumber,
}

/// The validation state of a [`BlockSubscription`] stream.
pub struct BlockSubscriptionState {
    /// The block number that the next item must have.
    next_block_num: BlockNumber,
    /// The commitments of the last validated block. It is `None` before the first item.
    previous: Option<PreviousBlock>,
}

/// The commitments of a validated block that the next block must agree with.
struct PreviousBlock {
    commitment: Word,
    protocol_config_commitment: Word,
}

impl RpcStreamMethod for BlockSubscription {
    type Item = SubscribedBlock;
    type State = BlockSubscriptionState;

    async fn open(&self, client: &GrpcClient) -> Result<RpcStream<Self::Item>, RpcError> {
        let request =
            proto::node::v1::BlockSubscriptionRequest { block_from: self.block_from.as_u32() };

        client
            .open_stream(
                RpcEndpoint::BlockSubscription,
                move |mut rpc_api| async move { rpc_api.block_subscription(request).await },
                decode_subscribed_block,
            )
            .await
    }

    fn initial_state(&self) -> Self::State {
        BlockSubscriptionState {
            next_block_num: self.block_from,
            previous: None,
        }
    }

    fn validate(&self, state: &mut Self::State, item: &Self::Item) -> Result<(), RpcError> {
        let header = item.block.header();
        let block_num = header.block_num();

        if block_num != state.next_block_num {
            return Err(RpcError::InvalidResponse(format!(
                "node sent block {block_num} but block {} is the next block of the subscription",
                state.next_block_num
            )));
        }

        if item.committed_chain_tip < block_num {
            return Err(RpcError::InvalidResponse(format!(
                "node sent block {block_num} with the committed chain tip {}",
                item.committed_chain_tip
            )));
        }

        if let Some(previous) = &state.previous
            && header.prev_block_commitment() != previous.commitment
        {
            return Err(RpcError::InvalidResponse(format!(
                "block {block_num} does not follow the block that the node sent before it"
            )));
        }

        // The caller cannot interpret a block without the configuration that its header commits to,
        // so the node must send each configuration that the caller has not seen.
        let expected = header.protocol_config_commitment();
        let is_new_config = state
            .previous
            .as_ref()
            .is_none_or(|previous| previous.protocol_config_commitment != expected);
        match &item.protocol_config {
            Some(protocol_config) => {
                let returned = protocol_config.to_commitment();
                if returned != expected {
                    return Err(RpcError::InvalidResponse(format!(
                        "node sent a protocol configuration with commitment {returned} for block {block_num}, which commits to {expected}"
                    )));
                }
            },
            None if is_new_config => {
                return Err(RpcError::ExpectedDataMissing(
                    "BlockSubscriptionResponse.protocol_config".to_string(),
                ));
            },
            None => {},
        }

        state.next_block_num = block_num.child();
        state.previous = Some(PreviousBlock {
            commitment: header.commitment(),
            protocol_config_commitment: expected,
        });

        Ok(())
    }
}

/// Decodes one item of the `BlockSubscription` stream.
fn decode_subscribed_block(
    response: proto::node::v1::BlockSubscriptionResponse,
) -> Result<SubscribedBlock, RpcError> {
    let block: SignedBlock = response
        .block
        .ok_or(RpcError::ExpectedDataMissing("BlockSubscriptionResponse.block".to_string()))?
        .decode_and_build_unchecked()?;

    let protocol_config: Option<ProtocolConfig> =
        response.protocol_config.map(DecodeMessageExt::decode_and_verify).transpose()?;

    Ok(SubscribedBlock {
        block,
        committed_chain_tip: BlockNumber::from(response.committed_chain_tip),
        protocol_config,
    })
}

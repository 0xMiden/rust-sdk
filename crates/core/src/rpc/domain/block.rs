use miden_protocol::block::{BlockNumber, SignedBlock};
use miden_protocol::protocol_config::ProtocolConfig;
use miden_protocol::vm::ExecutionProof;

// SUBSCRIBED BLOCK
// ================================================================================================

/// One committed block from a block subscription.
#[derive(Debug, Clone)]
pub struct SubscribedBlock {
    /// The signed block.
    pub block: SignedBlock,
    /// The committed chain tip of the node when it sent the block.
    pub committed_chain_tip: BlockNumber,
    /// The protocol configuration that the block header commits to. The first block of a
    /// subscription carries it. A later block carries it when its commitment differs from the
    /// commitment of the block before it.
    pub protocol_config: Option<ProtocolConfig>,
}

// SUBSCRIBED PROOF
// ================================================================================================

/// One block proof from a proof subscription.
#[derive(Debug, Clone)]
pub struct SubscribedProof {
    /// The block number of the proof.
    pub block_num: BlockNumber,
    /// The execution proof of the block.
    pub proof: ExecutionProof,
    /// The proven chain tip of the node when it sent the proof.
    pub proven_chain_tip: BlockNumber,
}

use miden_protocol::block::BlockNumber;
use miden_protocol::note::Nullifier;

// NULLIFIER UPDATE
// ================================================================================================

/// Represents a note that was consumed in the node at a certain block.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct NullifierUpdate {
    /// The nullifier of the consumed note.
    pub nullifier: Nullifier,
    /// The number of the block in which the note consumption was registered.
    pub block_num: BlockNumber,
}

use alloc::string::String;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::asset::Asset;
use miden_protocol::note::NoteId;

/// Represents node status info with fields converted into domain types.
pub struct RpcStatusInfo {
    pub version: String,
    pub genesis_commitment: Option<Word>,
    pub chain_tip: u32,
    pub block_producer: Option<BlockProducerStatusInfo>,
}

/// Represents block producer status info with fields converted into domain types.
pub struct BlockProducerStatusInfo {
    pub version: String,
    pub status: String,
    pub chain_tip: u32,
    pub mempool_stats: Option<MempoolStatsInfo>,
}

/// Represents mempool stats with fields converted into domain types.
pub struct MempoolStatsInfo {
    pub unbatched_transactions: u64,
    pub proposed_batches: u64,
    pub proven_batches: u64,
}

pub enum NetworkNoteStatus {
    /// The note is awaiting execution or being retried after transient failures.
    Pending,
    /// The note has been consumed by a transaction that was sent to the block producer.
    NullifierInflight,
    /// The note exceeded the maximum retry count and will not be retried.
    Discarded,
    /// The note's consuming transaction has been committed on-chain.
    NullifierCommitted,
}

impl core::fmt::Display for NetworkNoteStatus {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NetworkNoteStatus::Pending => write!(f, "Pending"),
            NetworkNoteStatus::NullifierInflight => write!(f, "NullifierInflight"),
            NetworkNoteStatus::Discarded => write!(f, "Discarded"),
            NetworkNoteStatus::NullifierCommitted => write!(f, "NullifierCommitted"),
        }
    }
}

/// Information about the processing status of a note submitted to the network.
///
/// This is returned by the `GetNetworkNoteStatus` RPC endpoint and provides details about how the
/// node is handling a note, including retry attempts and error diagnostics.
pub struct NetworkNoteStatusInfo {
    /// The current processing status of the note.
    pub status: NetworkNoteStatus,
    /// The error message from the most recent failed processing attempt, if any.
    pub last_error: Option<String>,
    /// The total number of times the node has attempted to process this note.
    pub attempt_count: u32,
    /// The block number at which the last processing attempt occurred, if any.
    pub last_attempt_block_num: Option<u32>,
    /// The fee sponsorship notes that the node binds to this note.
    pub sponsorships: Vec<NetworkNoteSponsorship>,
}

/// A fee sponsorship note that pays the fee for a network note.
pub struct NetworkNoteSponsorship {
    /// The ID of the sponsorship note.
    pub note_id: NoteId,
    /// The fee asset that the sponsorship note carries.
    pub fee_asset: Asset,
    /// The block number at or after which the reclaimer can reclaim the note. `None` if reclaim is
    /// disabled.
    pub reclaim_height: Option<u32>,
    /// The block number in which the sponsorship note was consumed. `None` while the note is not
    /// consumed.
    pub committed_block_num: Option<u32>,
    /// The error message from the most recent failed processing attempt of the sponsorship note, if
    /// any.
    pub last_error: Option<String>,
    /// The block number at which the last failed processing attempt of the sponsorship note
    /// occurred, if any.
    pub last_attempt_block_num: Option<u32>,
}

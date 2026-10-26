// RPC LIMITS
// ================================================================================================

const DEFAULT_NOTE_IDS_LIMIT: u32 = 100;
const DEFAULT_NULLIFIERS_LIMIT: u32 = 1000;
const DEFAULT_ACCOUNT_IDS_LIMIT: u32 = 1000;
const DEFAULT_NOTE_TAGS_LIMIT: u32 = 1000;

/// Domain type representing RPC endpoint limits.
///
/// These limits define the maximum number of items that can be sent in a single RPC request.
/// Exceeding these limits will result in the request being rejected by the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpcLimits {
    /// Maximum number of note IDs that can be sent in a single `GetNotesById` request.
    pub note_ids_limit: u32,
    /// Maximum number of nullifier prefixes that can be sent in a single `SyncNullifiers` request.
    pub nullifiers_limit: u32,
    /// Maximum number of account IDs that can be sent in a single `SyncTransactions` request.
    pub account_ids_limit: u32,
    /// Maximum number of note tags that can be sent in a single `SyncNotes` request.
    pub note_tags_limit: u32,
}

impl Default for RpcLimits {
    fn default() -> Self {
        Self {
            note_ids_limit: DEFAULT_NOTE_IDS_LIMIT,
            nullifiers_limit: DEFAULT_NULLIFIERS_LIMIT,
            account_ids_limit: DEFAULT_ACCOUNT_IDS_LIMIT,
            note_tags_limit: DEFAULT_NOTE_TAGS_LIMIT,
        }
    }
}

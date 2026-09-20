// RPC LIMITS
// ================================================================================================

use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

/// Key used to store RPC limits in the settings table.
pub(crate) const RPC_LIMITS_STORE_SETTING: &str = "rpc_limits";

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

impl Serializable for RpcLimits {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.note_ids_limit.write_into(target);
        self.nullifiers_limit.write_into(target);
        self.account_ids_limit.write_into(target);
        self.note_tags_limit.write_into(target);
    }
}

impl Deserializable for RpcLimits {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        Ok(Self {
            note_ids_limit: u32::read_from(source)?,
            nullifiers_limit: u32::read_from(source)?,
            account_ids_limit: u32::read_from(source)?,
            note_tags_limit: u32::read_from(source)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_limits_serialization_roundtrip() {
        let original = RpcLimits {
            note_ids_limit: 100,
            nullifiers_limit: 1000,
            account_ids_limit: 1000,
            note_tags_limit: 1000,
        };

        let bytes = original.to_bytes();
        let deserialized = RpcLimits::read_from_bytes(&bytes).expect("deserialization failed");

        assert_eq!(original, deserialized);
    }
}

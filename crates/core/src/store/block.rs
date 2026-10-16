// BLOCK RELEVANCE
// ================================================================================================

/// Expresses metadata about the block header.
#[derive(Debug, Clone)]
pub enum BlockRelevance {
    /// The block header includes notes that the client may consume.
    HasNotes,
    /// The block header does not contain notes relevant to the client.
    Irrelevant,
}

impl From<BlockRelevance> for bool {
    fn from(val: BlockRelevance) -> Self {
        match val {
            BlockRelevance::HasNotes => true,
            BlockRelevance::Irrelevant => false,
        }
    }
}

impl From<bool> for BlockRelevance {
    fn from(has_notes: bool) -> Self {
        if has_notes {
            BlockRelevance::HasNotes
        } else {
            BlockRelevance::Irrelevant
        }
    }
}

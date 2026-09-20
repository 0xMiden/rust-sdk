use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteId, Nullifier};
use miden_protocol::transaction::{InputNoteCommitment, TransactionHeader};

use super::note::CommittedNote;

// TRANSACTION RECORD
// ================================================================================================

/// Contains information about a transaction that got included in the chain at a specific block
/// number.
#[derive(Debug, Clone)]
pub struct TransactionRecord {
    /// Block number in which the transaction was included.
    pub block_num: BlockNumber,
    /// A transaction header.
    pub transaction_header: TransactionHeader,
    /// Output notes with inclusion proofs, as returned by the node's `SyncTransactions` response.
    /// Does not include erased notes.
    pub output_notes: Vec<CommittedNote>,
    /// Output notes that were erased by same-batch note erasure.
    pub erased_output_notes: Vec<NoteHeader>,
    /// Maps each consumed input note's nullifier to its note id, for public notes the node could
    /// resolve. Lets a client recover, by id, a consumed note it never tracked. Empty for
    /// private/unresolvable inputs.
    // TODO: perhaps we might want to rename this field (see
    // https://github.com/0xMiden/node/pull/2304#discussion_r3511308376)
    pub(crate) consumed_note_refs: Vec<(Nullifier, NoteId)>,
}

impl TransactionRecord {
    /// Returns the `(nullifier, note_id)` references of the public input notes this transaction
    /// consumed, letting a client fetch by id consumed notes it never tracked.
    ///
    /// Only yields references whose nullifier appears in the transaction header's input notes: a
    /// reference the node can't tie to an actually-consumed input is dropped, so a misbehaving node
    /// can't attribute an unrelated note to this transaction's account.
    pub fn trusted_consumed_note_refs(&self) -> impl Iterator<Item = (Nullifier, NoteId)> + '_ {
        let consumed_nullifiers: BTreeSet<Nullifier> = self
            .transaction_header
            .input_notes()
            .iter()
            .map(InputNoteCommitment::nullifier)
            .collect();
        self.consumed_note_refs
            .iter()
            .copied()
            .filter(move |(nullifier, _)| consumed_nullifiers.contains(nullifier))
    }
}

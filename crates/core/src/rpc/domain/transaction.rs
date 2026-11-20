use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteId, Nullifier};
use miden_protocol::transaction::{
    InputNoteCommitment,
    TransactionHeader,
    TransactionId,
    TransactionLogDetails,
    TransactionLogs,
};

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
    consumed_note_refs: Vec<(Nullifier, NoteId)>,
}

impl TransactionRecord {
    /// Creates a new [`TransactionRecord`].
    pub fn new(
        block_num: BlockNumber,
        transaction_header: TransactionHeader,
        output_notes: Vec<CommittedNote>,
        erased_output_notes: Vec<NoteHeader>,
        consumed_note_refs: Vec<(Nullifier, NoteId)>,
    ) -> Self {
        Self {
            block_num,
            transaction_header,
            output_notes,
            erased_output_notes,
            consumed_note_refs,
        }
    }

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

// TRANSACTION LOGS INFO
// ================================================================================================

/// Contains the logs of a committed transaction and the position of the transaction in the chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionLogsInfo {
    /// ID of the transaction that emitted the logs.
    pub transaction_id: TransactionId,
    /// ID of the account that executed the transaction.
    pub account_id: AccountId,
    /// Block number in which the transaction was included.
    pub block_num: BlockNumber,
    /// Index of the transaction in the block.
    pub transaction_index: u32,
    /// The logs of a public account, or the commitment to the logs of a private account.
    pub log_details: TransactionLogDetails,
}

impl TransactionLogsInfo {
    /// Returns the commitment to the transaction logs.
    pub fn logs_commitment(&self) -> Word {
        self.log_details.commitment()
    }

    /// Returns the transaction logs, or `None` if the node holds only their commitment.
    pub fn logs(&self) -> Option<&TransactionLogs> {
        match &self.log_details {
            TransactionLogDetails::Public(logs) => Some(logs),
            TransactionLogDetails::Private(_) => None,
        }
    }
}

//! This module defines common structs to be used within the [`Store`](crate::store::Store) for
//! notes that are available to be consumed ([`InputNoteRecord`]) and notes that have been produced
//! as a result of executing a transaction ([`OutputNoteRecord`]).
//!
//! Both structs are similar in terms of the data they carry, but are differentiated semantically as
//! they are involved in very different flows. As such, known states are modeled differently for the
//! two structures, with [`InputNoteRecord`] having states described by the [`InputNoteState`] enum.
//!
//! ## Type conversion
//!
//! We also facilitate converting from/into [`InputNote`](miden_protocol::transaction::InputNote) /
//! [`Note`](miden_protocol::note::Note), although this is not always possible. Check both
//! [`InputNoteRecord`]'s and [`OutputNoteRecord`]'s documentation for more details about this.

use miden_protocol::block::BlockNumber;
use miden_protocol::note::NoteDetailsCommitment;

mod input_note_record;
mod output_note_record;

pub use input_note_record::{InputNoteRecord, InputNoteState};
pub use output_note_record::{NoteExportType, OutputNoteRecord, OutputNoteState};

/// Contains structures that model all states in which an input note can be.
pub mod input_note_states {
    pub use super::input_note_record::{
        CommittedNoteState,
        ConsumedAuthenticatedLocalNoteState,
        ConsumedExternalNoteState,
        ConsumedUnauthenticatedLocalNoteState,
        ExpectedNoteState,
        InputNoteState,
        InvalidNoteState,
        NoteSubmissionData,
        ProcessingAuthenticatedNoteState,
        ProcessingUnauthenticatedNoteState,
        UnverifiedNoteState,
    };
}

// INPUT NOTE CURSOR
// ================================================================================================

/// Identifies a position in the per-account consumption order of input notes.
///
/// Obtained from a record returned by
/// [`Store::get_input_note_after`](crate::store::Store::get_input_note_after) and passed back to
/// fetch the note that follows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct InputNoteCursor {
    consumed_block_height: BlockNumber,
    consumed_tx_order: u32,
    details_commitment: NoteDetailsCommitment,
}

impl InputNoteCursor {
    /// Returns the cursor pointing at `record`, or `None` if the note is not consumed.
    pub fn from_record(record: &InputNoteRecord) -> Option<Self> {
        Some(Self {
            consumed_block_height: record.state().consumed_block_height()?,
            consumed_tx_order: record.state().consumed_tx_order()?,
            details_commitment: record.details_commitment(),
        })
    }

    /// Returns the block height at which the note was consumed.
    pub fn consumed_block_height(&self) -> BlockNumber {
        self.consumed_block_height
    }

    /// Returns the per-account position of the consuming transaction within the block.
    pub fn consumed_tx_order(&self) -> u32 {
        self.consumed_tx_order
    }

    /// Returns the commitment to the note's details.
    pub fn details_commitment(&self) -> NoteDetailsCommitment {
        self.details_commitment
    }
}

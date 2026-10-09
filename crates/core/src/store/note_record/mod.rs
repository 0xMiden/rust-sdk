//! This module defines common structs to be used within the [`Store`](crate::store::Store) for
//! notes that are available to be consumed ([`InputNoteRecord`]) and notes that have been produced
//! as a result of executing a transaction ([`OutputNoteRecord`]).
//!
//! Both structs are similar in terms of the data they carry, but are differentiated semantically as
//! they are involved in very different flows. As such, known states are modeled differently for the
//! two structures, with [`InputNoteRecord`] having states described by the [`InputNoteState`] enum.
//!
//! ## Serialization / Deserialization
//!
//! We provide serialization and deserialization support via [`Serializable`] and [`Deserializable`]
//! traits implementations.
//!
//! ## Type conversion
//!
//! We also facilitate converting from/into [`InputNote`](miden_protocol::transaction::InputNote) /
//! [`Note`](miden_protocol::note::Note), although this is not always possible. Check both
//! [`InputNoteRecord`]'s and [`OutputNoteRecord`]'s documentation for more details about this.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteDetailsCommitment, NoteId, NoteScriptRoot, Nullifier};
use thiserror::Error;

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

// NOTE RECORD ERROR
// ================================================================================================

/// Errors generated from note records.
#[derive(Debug, Error)]
pub enum NoteRecordError {
    /// Error generated during conversion of note record.
    #[error("note record conversion error: {0}")]
    ConversionError(String),
    /// Note record isn't consumable.
    #[error("note not consumable: {0}")]
    NoteNotConsumable(String),
    /// Invalid state transition.
    #[error("invalid state transition: {0}")]
    InvalidStateTransition(String),
    /// Error generated during a state transition.
    #[error("state transition error: {0}")]
    StateTransitionError(String),
}

impl From<NoteRecordError> for String {
    fn from(err: NoteRecordError) -> String {
        err.to_string()
    }
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

// NOTE FILTER
// ================================================================================================

/// Filters for narrowing the set of notes returned by the client's store.
#[derive(Debug, Clone)]
pub enum NoteFilter {
    /// Return a list of all notes ([`InputNoteRecord`] or [`OutputNoteRecord`]).
    All,
    /// Return a list of committed notes ([`InputNoteRecord`] or [`OutputNoteRecord`]). These
    /// represent notes that the blockchain has included in a block.
    Committed,
    /// Filter by consumed notes ([`InputNoteRecord`] or [`OutputNoteRecord`]). notes that have been
    /// used as inputs in transactions.
    Consumed,
    /// Return a list of expected notes ([`InputNoteRecord`] or [`OutputNoteRecord`]). These
    /// represent notes for which the store doesn't have anchor data.
    Expected,
    /// Return a list containing any notes that match with the provided [`NoteId`] vector.
    List(Vec<NoteId>),
    /// Return a list containing any notes whose details commitment matches one of the provided
    /// [`NoteDetailsCommitment`] vector. Unlike [`NoteFilter::List`], this matches the
    /// metadata-independent details commitment, so it also resolves metadata-less notes (which have
    /// a NULL `note_id`).
    DetailsCommitments(Vec<NoteDetailsCommitment>),
    /// Return a list containing any notes that match the provided [`Nullifier`] vector.
    Nullifiers(Vec<Nullifier>),
    /// Return a list of notes that are currently being processed. This filter doesn't apply to
    /// output notes.
    Processing,
    /// Return a list containing any notes whose script root matches one of the provided
    /// [`NoteScriptRoot`]s. Notes whose script isn't known (e.g. partial output notes) never match.
    ScriptRoots(Vec<NoteScriptRoot>),
    /// Return a list containing the note that matches with the provided [`NoteId`]. The query will
    /// return an error if the note isn't found.
    Unique(NoteId),
    /// Return a list containing notes that haven't been nullified yet, this includes expected,
    /// committed, processing and unverified notes.
    Unspent,
    /// Return a list containing notes with unverified inclusion proofs. This filter doesn't apply
    /// to output notes.
    Unverified,
}

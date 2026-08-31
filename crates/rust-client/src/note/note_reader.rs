//! Provides a lazy iterator over consumed input notes.

use alloc::sync::Arc;

use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;

use crate::ClientError;
use crate::store::{InputNoteCursor, InputNoteRecord, NoteFilter, Store};

/// A lazy iterator over consumed input notes for a specific consumer account.
///
/// Each call to [`InputNoteReader::next`] executes a store query and returns the
/// next matching note. Use builder methods to configure filters before iterating.
///
/// # Ordering
///
/// Notes are returned in on-chain consumption order: first by block number, then by
/// per-account transaction order within the block. Notes consumed by the same transaction
/// are returned in a deterministic order that is consistent across calls.
pub struct InputNoteReader {
    store: Arc<dyn Store>,
    consumer: AccountId,
    block_range: Option<(BlockNumber, BlockNumber)>,
    cursor: Option<InputNoteCursor>,
}

impl InputNoteReader {
    /// Creates a new `InputNoteReader` that iterates over consumed input notes
    /// for the given consumer account.
    ///
    /// The consumer is required because ordering is only guaranteed among notes
    /// consumed by the same account.
    pub fn new(store: Arc<dyn Store>, consumer: AccountId) -> Self {
        Self {
            store,
            consumer,
            block_range: None,
            cursor: None,
        }
    }

    /// Restricts iteration to notes consumed within the given block range (inclusive).
    #[must_use]
    pub fn in_block_range(mut self, from: BlockNumber, to: BlockNumber) -> Self {
        self.block_range = Some((from, to));
        self
    }

    /// Resets the iterator to the beginning.
    pub fn reset(&mut self) {
        self.cursor = None;
    }

    /// Returns the next consumed input note, or `None` when all matching notes have been
    /// returned.
    ///
    /// Each call executes a single store query.
    pub async fn next(&mut self) -> Result<Option<InputNoteRecord>, ClientError> {
        let (block_start, block_end) = match self.block_range {
            Some((from, to)) => (Some(from), Some(to)),
            None => (None, None),
        };

        // TODO: The note filter should be configurable instead of hardcoding `NoteFilter::Consumed`
        let note = self
            .store
            .get_input_note_after(
                NoteFilter::Consumed,
                self.consumer,
                block_start,
                block_end,
                self.cursor,
            )
            .await
            .map_err(ClientError::StoreError)?;

        if let Some(note) = &note {
            // A note with no position cannot move the cursor forward, so silently keeping or
            // clearing it would either return this same note forever or restart the walk.
            let cursor = InputNoteCursor::from_record(note).ok_or_else(|| {
                ClientError::MissingNoteConsumptionPosition(note.details_commitment().as_word())
            })?;
            self.cursor = Some(cursor);
        }
        Ok(note)
    }
}

//! Defines the mock note transport node that the client tests and the protobuf encodings share.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteId, NoteInclusionProof, NoteTag};

use crate::note_transport::{NoteInfo, NoteTransportCursor};

/// Mock Note Transport Node
///
/// Simulates the functionality of the note transport node.
#[derive(Clone)]
pub struct MockNoteTransportNode {
    notes: BTreeMap<NoteTag, Vec<(NoteInfo, NoteTransportCursor)>>,
    nonce: u64,
    next_sequence: u64,
    /// Optional per-response batch cap; if `Some(n)`, `get_notes` returns at most `n` entries
    /// (total, across all tags) in one call. Used to exercise client-side pagination drain loops.
    /// `None` = unbounded (legacy behavior).
    max_batch: Option<usize>,
    /// Notes stored through the with-proof path, with the block their proof named.
    proven_notes: BTreeMap<NoteId, BlockNumber>,
}

impl MockNoteTransportNode {
    pub fn new() -> Self {
        Self {
            notes: BTreeMap::default(),
            nonce: 1,
            next_sequence: 1,
            max_batch: None,
            proven_notes: BTreeMap::default(),
        }
    }

    /// Builds a mock from the state that [`Self::notes`], [`Self::nonce`] and
    /// [`Self::next_sequence`] return. An encoder uses this to rebuild a mock that it encoded in
    /// its own format. The batch cap and the proven notes are not part of that state.
    pub fn from_parts(
        notes: BTreeMap<NoteTag, Vec<(NoteInfo, NoteTransportCursor)>>,
        nonce: u64,
        next_sequence: u64,
    ) -> Self {
        Self {
            notes,
            nonce,
            next_sequence,
            max_batch: None,
            proven_notes: BTreeMap::default(),
        }
    }

    /// Returns the stored notes of each tag with their cursors.
    pub fn notes(&self) -> &BTreeMap<NoteTag, Vec<(NoteInfo, NoteTransportCursor)>> {
        &self.notes
    }

    /// Returns the nonce of the mock.
    pub fn nonce(&self) -> u64 {
        self.nonce
    }

    /// Returns the sequence number that the next stored note gets.
    pub fn next_sequence(&self) -> u64 {
        self.next_sequence
    }

    /// Build a mock that caps each `get_notes` response at `max_batch` entries.
    pub fn with_max_batch(max_batch: usize) -> Self {
        Self {
            max_batch: Some(max_batch),
            ..Self::new()
        }
    }

    /// Stores `info` under `tag` at the next cursor position.
    fn push(&mut self, tag: NoteTag, info: NoteInfo) {
        let cursor = NoteTransportCursor {
            nonce: self.nonce,
            sequence: self.next_sequence,
        };
        self.next_sequence += 1;
        self.notes.entry(tag).or_default().push((info, cursor));
    }

    /// Seed a note relayed with its inclusion proof. The real service verifies the proof against
    /// its node; the mock only records the proof's block and serves it as the commitment block.
    ///
    /// The real service stores a note only once. The mock also ignores a note with an id that it
    /// already stores.
    pub fn add_note_with_proof(
        &mut self,
        header: NoteHeader,
        details_bytes: Vec<u8>,
        inclusion_proof: &NoteInclusionProof,
    ) {
        if self.contains_note(&header.id()) {
            return;
        }
        let block_num = inclusion_proof.location().block_num();
        self.proven_notes.insert(header.id(), block_num);
        self.add_note_after(header, details_bytes, Some(block_num));
    }

    /// Returns whether the mock stores a note with `note_id`.
    fn contains_note(&self, note_id: &NoteId) -> bool {
        self.notes.values().flatten().any(|(info, _)| info.header.id() == *note_id)
    }

    /// Returns the block named by the proof a note was stored with, or `None` when the note was not
    /// stored through the with-proof path.
    pub fn proven_block(&self, note_id: &NoteId) -> Option<BlockNumber> {
        self.proven_notes.get(note_id).copied()
    }

    pub fn add_note(&mut self, header: NoteHeader, details_bytes: Vec<u8>) {
        self.add_note_after(header, details_bytes, None);
    }

    /// Seed a note that carries a commitment block floor, as a transport that stored the note
    /// without verifying a proof would serve it.
    pub fn add_note_after(
        &mut self,
        header: NoteHeader,
        details_bytes: Vec<u8>,
        block_hint: Option<BlockNumber>,
    ) {
        let tag = header.metadata().tag();
        self.push(tag, NoteInfo { header, details_bytes, block_hint });
    }

    /// Seed a note under an arbitrary transport tag key, regardless of the note's own tag.
    pub fn add_note_with_tag_key(
        &mut self,
        tag: NoteTag,
        header: NoteHeader,
        details_bytes: Vec<u8>,
    ) {
        self.push(tag, NoteInfo { header, details_bytes, block_hint: None });
    }

    pub fn get_notes(
        &self,
        tags: &[NoteTag],
        cursor: Option<NoteTransportCursor>,
    ) -> (Vec<NoteInfo>, NoteTransportCursor) {
        self.notes_up_to(tags, cursor, self.head())
    }

    /// Returns the position of the last stored note.
    pub fn head(&self) -> NoteTransportCursor {
        NoteTransportCursor {
            nonce: self.nonce,
            sequence: self.next_sequence - 1,
        }
    }

    /// Returns the notes after `cursor` and up to `sync_to`.
    pub fn notes_up_to(
        &self,
        tags: &[NoteTag],
        cursor: Option<NoteTransportCursor>,
        sync_to: NoteTransportCursor,
    ) -> (Vec<NoteInfo>, NoteTransportCursor) {
        // A cursor from another database starts from the first retained note.
        let after = cursor.filter(|cursor| cursor.nonce == self.nonce).map_or(0, |c| c.sequence);
        let mut collected: Vec<(NoteInfo, NoteTransportCursor)> = tags
            .iter()
            .filter_map(|tag| self.notes.get(tag))
            .flatten()
            .filter(|(_, tcursor)| tcursor.sequence > after && tcursor.sequence <= sync_to.sequence)
            .cloned()
            .collect();

        // Deterministic ordering across tags: sort by cursor ascending so the client sees notes in
        // per-cursor order regardless of tag iteration order, matching the real server's `ORDER BY
        // seq ASC`.
        collected.sort_by_key(|(_, c)| c.sequence);

        // Apply the batch cap, if configured.
        if let Some(max) = self.max_batch {
            collected.truncate(max);
        }

        // Matches the real server's contract (`rcursor = max(cursor, max_seq_returned)`), so an
        // empty batch returns the caller's own position.
        let sequence = collected.last().map_or(after, |(_, c)| c.sequence);
        let notes = collected.into_iter().map(|(n, _)| n).collect();
        (notes, NoteTransportCursor { nonce: self.nonce, sequence })
    }
}

impl Default for MockNoteTransportNode {
    fn default() -> Self {
        Self::new()
    }
}

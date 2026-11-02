//! Defines the note transport types shared by the client and the store implementations.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteTag};

/// Note transport cursor
///
/// Identifies a position in the note transport service's stored-note sequence.
///
/// The sequence is global across tags in one service database. Compare sequences only when their
/// nonces match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteTransportCursor {
    /// Identifies the service database.
    pub nonce: u64,
    /// Position in the stored-note sequence of the service database.
    pub sequence: u64,
}

/// Stored cursor of the note transport fetch and the tags that received all notes up to it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteTransportSyncState {
    /// Position up to which the covered tags received all notes.
    pub cursor: NoteTransportCursor,
    /// Tags that received all notes up to the cursor.
    pub covered_tags: BTreeSet<NoteTag>,
}

/// Information about a note fetched from the note transport network
#[derive(Debug, Clone)]
pub struct NoteInfo {
    /// Note header.
    pub header: NoteHeader,
    /// Serialized note details.
    pub details_bytes: Vec<u8>,
    /// Block from which the recipient starts scanning for the note's on-chain commitment. This is
    /// either an unverified sender hint or the exact block verified by a proof-aware transport.
    /// `None` applies the recipient's default lookback window.
    pub block_hint: Option<BlockNumber>,
}

impl NoteInfo {
    /// Builds a [`NoteInfo`] without a block hint (`block_hint` is `None`).
    ///
    /// Use the [`NoteInfo::block_hint`] field directly to attach a hint.
    pub fn new(header: NoteHeader, details_bytes: Vec<u8>) -> Self {
        Self { header, details_bytes, block_hint: None }
    }
}

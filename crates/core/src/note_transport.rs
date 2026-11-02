//! Defines the note transport types shared by the client and the store implementations.

use alloc::collections::BTreeSet;

use miden_protocol::note::NoteTag;
use miden_protocol::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

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

impl Serializable for NoteTransportCursor {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        (self.nonce, self.sequence).write_into(target);
    }
}

impl Deserializable for NoteTransportCursor {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let (nonce, sequence) = <(u64, u64)>::read_from(source)?;
        Ok(Self { nonce, sequence })
    }
}

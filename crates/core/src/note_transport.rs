//! Defines the note transport types shared by the client and the store implementations.

use miden_protocol::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

/// Settings key for the unused aggregate transport cursor.
#[deprecated(since = "0.17.1", note = "note transport stores a cursor for each tag")]
pub const NOTE_TRANSPORT_CURSOR_STORE_SETTING: &str = "note_transport_cursor";

/// Note transport cursor
///
/// Identifies a position in the note transport service's stored-note sequence.
///
/// The sequence is global across tags in one service database. Compare sequences only when their
/// nonces match.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct NoteTransportCursor(Option<(u64, u64)>);

impl NoteTransportCursor {
    /// Returns the cursor that starts from the first retained note.
    pub fn init() -> Self {
        Self(None)
    }

    /// Builds a cursor from the nonce and sequence returned by the transport service.
    pub fn from_parts(nonce: u64, sequence: u64) -> Self {
        Self(Some((nonce, sequence)))
    }

    /// Returns the nonce and sequence, or `None` for the initial cursor.
    pub fn parts(&self) -> Option<(u64, u64)> {
        self.0
    }
}

impl Serializable for NoteTransportCursor {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.0.write_into(target);
    }
}

impl Deserializable for NoteTransportCursor {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        Ok(Self(Option::<(u64, u64)>::read_from(source)?))
    }
}

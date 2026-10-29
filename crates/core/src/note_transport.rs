//! Defines the note transport types shared by the client and the store implementations.

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

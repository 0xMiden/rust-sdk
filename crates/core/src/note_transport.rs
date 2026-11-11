//! Defines the note transport types shared by the client and the store implementations.

use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{Note, NoteDetails, NoteHeader, NoteInclusionProof, NoteTag};

use crate::errors::NoteTransportError;

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

// NOTE TRANSPORT CLIENT
// ================================================================================================

/// The main transport client trait for sending and receiving private notes.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait NoteTransportClient: Send + Sync {
    /// Sends a note together with its inclusion proof.
    ///
    /// The transport carries the proof to the network. The network verifies `inclusion_proof`
    /// before it stores the note and relays the exact commitment block to the recipient.
    async fn send_note_with_proof(
        &self,
        note: TransportNote,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), NoteTransportError>;

    /// Fetches a page of notes after `cursor` and up to `sync_to` for the given tags.
    ///
    /// Without `cursor`, the page starts from the first retained note. Without `sync_to`, the
    /// service uses its current head as the bound. The page returns the bound that the service
    /// used. A cursor from another service database restarts from the first retained note.
    async fn fetch_notes_page(
        &self,
        tags: &[NoteTag],
        cursor: Option<NoteTransportCursor>,
        sync_to: Option<NoteTransportCursor>,
    ) -> Result<NoteTransportPage, NoteTransportError>;
}

/// A page of notes from the transport service.
pub struct NoteTransportPage {
    /// Notes in global sequence order.
    pub notes: Vec<NoteInfo>,
    /// Position of the last returned note. An empty page keeps the request position.
    pub cursor: NoteTransportCursor,
    /// Indicates that the requested tags have another page up to `sync_to`.
    pub has_more: bool,
    /// Inclusive upper bound of the request. When `has_more` is `false`, the page completes the
    /// requested tags up to this bound.
    pub sync_to: NoteTransportCursor,
}

// NOTE INFO
// ================================================================================================

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

// TRANSPORT NOTE
// ================================================================================================

/// The part of a note that the note transport network sends to a recipient.
///
/// The transport sends the original header and details. It does not send note attachments. The
/// constructor verifies that the header commits to the details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportNote {
    header: NoteHeader,
    details: NoteDetails,
}

impl TransportNote {
    /// Creates a transport note from matching note parts.
    pub fn new(header: NoteHeader, details: NoteDetails) -> Result<Self, NoteTransportError> {
        let header_commitment = header.details_commitment();
        let details_commitment = details.commitment();
        if header_commitment != details_commitment {
            return Err(NoteTransportError::NoteDetailsMismatch {
                header: header_commitment,
                details: details_commitment,
            });
        }

        Ok(Self { header, details })
    }

    /// Returns the note header.
    pub fn header(&self) -> &NoteHeader {
        &self.header
    }

    /// Returns the note details.
    pub fn details(&self) -> &NoteDetails {
        &self.details
    }

    /// Returns the note header and details.
    pub fn into_parts(self) -> (NoteHeader, NoteDetails) {
        (self.header, self.details)
    }
}

impl From<Note> for TransportNote {
    fn from(note: Note) -> Self {
        let header = *note.header();
        let details = NoteDetails::from(note);
        Self { header, details }
    }
}

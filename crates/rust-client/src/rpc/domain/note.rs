use alloc::collections::BTreeMap;
use alloc::format;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::note::{
    Note,
    NoteAttachments,
    NoteDetails,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteTag,
    NoteType,
};

use crate::rpc::{RpcConversionError, RpcError};

// SYNC NOTE
// ================================================================================================

/// Represents a single block's worth of note sync data from the `SyncNotesResponse`.
#[derive(Debug, Clone)]
pub struct SyncNotesBlock {
    /// Block header containing the matching notes.
    pub block_header: BlockHeader,
    /// MMR path for verifying the block's inclusion in the MMR at `block_to`.
    pub mmr_path: MerklePath,
    /// Notes matching the requested tags in this block, keyed by note ID.
    pub notes: BTreeMap<NoteId, CommittedNote>,
}

// SYNCED NOTE
// ================================================================================================

/// A block's worth of notes resolved by
/// [`NodeRpcClient::sync_notes_with_content`](crate::rpc::NodeRpcClient::sync_notes_with_content).
///
/// Unlike [`SyncNotesBlock`] (the raw `SyncNotes` response), each note here also carries its
/// attachments and, for a fetched public note, its details, so no re-joining by note ID is needed.
#[derive(Debug, Clone)]
pub struct ResolvedSyncNotesBlock {
    /// Block header containing the matching notes.
    pub block_header: BlockHeader,
    /// MMR path for verifying the block's inclusion in the MMR at `block_to`.
    pub mmr_path: MerklePath,
    /// Notes matching the requested tags in this block, keyed by note ID.
    pub notes: BTreeMap<NoteId, SyncedNote>,
}

/// Everything resolved about a single note during a notes sync: its identity, metadata, and
/// inclusion proof (always present, from `SyncNotes`), its attachments, and the public note body
/// when it was fetched via `GetNotesById`.
#[derive(Debug, Clone)]
pub struct SyncedNote {
    /// Note ID of the synced note, as reported by `SyncNotes`.
    pub note_id: NoteId,
    /// The note's full metadata, as reported by `SyncNotes`.
    pub metadata: NoteMetadata,
    /// Inclusion proof for the note in the block, as reported by `SyncNotes`.
    pub inclusion_proof: NoteInclusionProof,
    /// The public note's body, fetched via `GetNotesById`. `None` for a private note, and for a
    /// public note whose body was not requested or not returned.
    pub details: Option<NoteDetails>,
    /// The note's attachments, either carried in full by the sync record or fetched via
    /// `GetNotesById`. Empty for a note whose metadata advertises none.
    pub attachments: NoteAttachments,
}

impl SyncedNote {
    /// Pairs a sync record with the content resolved for it, checking that the content is
    /// consistent with the record:
    ///
    /// - Only a public note can have a body. The converse is not checked, since a public note
    ///   legitimately has no body whenever its body was not requested.
    /// - The attachments must hash to the metadata's attachments commitment. This also catches a
    ///   note advertising attachments whose content never arrived, which would be unconsumable.
    ///
    /// Both sides of that check come from the node, so it is a consistency check between its
    /// responses. The note is authenticated by a consumer recomputing its id and inclusion proof.
    ///
    /// A rejection concerns a single note, not the response as a whole:
    /// [`NodeRpcClient::sync_notes_with_content`](crate::rpc::NodeRpcClient::sync_notes_with_content)
    /// skips the offending note with a warning instead of failing the sync, since content
    /// availability can be influenced by the note's creator.
    pub fn new(
        committed: CommittedNote,
        details: Option<NoteDetails>,
        attachments: NoteAttachments,
    ) -> Result<Self, RpcError> {
        if details.is_some() && committed.note_type() != NoteType::Public {
            return Err(RpcError::InvalidResponse(format!(
                "a note body was returned for private note {}",
                committed.note_id()
            )));
        }

        if attachments.to_commitment() != committed.metadata().attachments_commitment() {
            return Err(RpcError::InvalidResponse(format!(
                "the attachments resolved for note {} do not match the note's attachments \
                 commitment",
                committed.note_id()
            )));
        }

        let CommittedNote {
            note_id,
            metadata,
            inclusion_proof,
            attachments: _,
        } = committed;

        Ok(Self {
            note_id,
            metadata,
            inclusion_proof,
            details,
            attachments,
        })
    }

    /// Returns the number of the block in which the note was committed.
    pub fn block_num(&self) -> BlockNumber {
        self.inclusion_proof.location().block_num()
    }

    /// Consumes the synced note and returns its sync record together with the attachment content
    /// resolved for it. The note body, which the record does not hold, is dropped.
    ///
    /// The returned record reports its attachments as resolved, so
    /// [`CommittedNote::needs_attachment_fetch`] is always `false` for it. A note without
    /// attachments carries an empty set.
    pub fn into_committed_note(self) -> CommittedNote {
        CommittedNote {
            note_id: self.note_id,
            metadata: self.metadata,
            inclusion_proof: self.inclusion_proof,
            attachments: Some(self.attachments),
        }
    }
}

// COMMITTED NOTE
// ================================================================================================

/// Represents a committed note, returned as part of a `SyncNotesResponse`.
#[derive(Debug, Clone)]
pub struct CommittedNote {
    /// Note ID of the committed note.
    note_id: NoteId,
    /// Note metadata. Sync responses always carry the full [`NoteMetadata`]: header fields plus
    /// attachment scheme markers and the attachments commitment.
    metadata: NoteMetadata,
    /// Inclusion proof for the note in the block.
    inclusion_proof: NoteInclusionProof,
    /// The note's attachment content, when the source reporting the note carried every attachment
    /// verbatim. See [`CommittedNote::attachments`].
    attachments: Option<NoteAttachments>,
}

impl CommittedNote {
    pub fn new(
        note_id: NoteId,
        metadata: NoteMetadata,
        inclusion_proof: NoteInclusionProof,
    ) -> Self {
        Self {
            note_id,
            metadata,
            inclusion_proof,
            attachments: None,
        }
    }

    /// Records the note's attachment content, for a source that reports every attachment verbatim.
    ///
    /// # Errors
    ///
    /// Returns an error if the content does not hash to the metadata's attachments commitment. Such
    /// content would turn [`CommittedNote::needs_attachment_fetch`] off for a note whose real
    /// content was never obtained, leaving it to be dropped for good by the consistency check in
    /// [`SyncedNote::new`] instead of being fetched.
    pub fn with_attachments(
        mut self,
        attachments: NoteAttachments,
    ) -> Result<Self, RpcConversionError> {
        if attachments.to_commitment() != self.metadata.attachments_commitment() {
            return Err(RpcConversionError::InvalidField(format!(
                "attachments recorded for note {} do not match its attachments commitment",
                self.note_id,
            )));
        }

        self.attachments = Some(attachments);
        Ok(self)
    }

    pub fn note_id(&self) -> &NoteId {
        &self.note_id
    }

    pub fn note_type(&self) -> NoteType {
        self.metadata.note_type()
    }

    pub fn tag(&self) -> NoteTag {
        self.metadata.tag()
    }

    pub fn sender(&self) -> AccountId {
        self.metadata.sender()
    }

    /// Returns the full note metadata.
    pub fn metadata(&self) -> &NoteMetadata {
        &self.metadata
    }

    /// Returns `true` if the note's metadata advertises at least one attachment.
    pub fn has_attachments(&self) -> bool {
        self.metadata.has_attachments()
    }

    /// Returns the note's attachment content, `Some` when the reporting source carried every
    /// attachment verbatim.
    ///
    /// `None` means at least one attachment must be fetched via `GetNotesById`, or that the source
    /// reports no attachment content at all, as `SyncTransactions` inclusion proofs do.
    pub fn attachments(&self) -> Option<&NoteAttachments> {
        self.attachments.as_ref()
    }

    /// Returns `true` if the note's attachment content has to be fetched via `GetNotesById`: its
    /// metadata advertises attachments and the source reporting the note did not carry them all.
    pub fn needs_attachment_fetch(&self) -> bool {
        self.has_attachments() && self.attachments.is_none()
    }

    pub fn inclusion_proof(&self) -> &NoteInclusionProof {
        &self.inclusion_proof
    }

    /// Returns the number of the block in which the note was committed.
    pub fn block_num(&self) -> BlockNumber {
        self.inclusion_proof.location().block_num()
    }
}

// FETCHED NOTE
// ================================================================================================

/// Describes the possible responses from the `GetNotesById` endpoint for a single note.
#[allow(clippy::large_enum_variant)]
pub enum FetchedNote {
    /// Details for a private note include its ID, metadata, attachments and inclusion proof. Other
    /// details needed to consume the note are expected to be stored locally, off-chain.
    ///
    /// Attachments are a public extension of the note and are stored on-chain even for private
    /// notes, so the node returns them here; they are needed to reconstruct the correct note ID.
    Private(NoteId, NoteMetadata, NoteAttachments, NoteInclusionProof),
    /// Contains the full [`Note`] object alongside its [`NoteInclusionProof`].
    Public(Note, NoteInclusionProof),
}

impl FetchedNote {
    /// Returns the note's inclusion details.
    pub fn inclusion_proof(&self) -> &NoteInclusionProof {
        match self {
            FetchedNote::Private(_, _, _, inclusion_proof)
            | FetchedNote::Public(_, inclusion_proof) => inclusion_proof,
        }
    }

    /// Returns the note's metadata.
    pub fn metadata(&self) -> &NoteMetadata {
        match self {
            FetchedNote::Private(_, metadata, ..) => metadata,
            FetchedNote::Public(note, _) => note.metadata(),
        }
    }

    /// Returns the note's attachments.
    pub fn attachments(&self) -> &NoteAttachments {
        match self {
            FetchedNote::Private(_, _, attachments, _) => attachments,
            FetchedNote::Public(note, _) => note.attachments(),
        }
    }

    /// Returns the note's ID.
    pub fn id(&self) -> NoteId {
        match self {
            FetchedNote::Private(note_id, ..) => *note_id,
            FetchedNote::Public(note, _) => note.id(),
        }
    }
}

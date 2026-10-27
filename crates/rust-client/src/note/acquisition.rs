//! Fallible retention acknowledgements for note import candidates.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteDetailsCommitment, NoteId, NoteMetadata, NoteTag, Nullifier};
use thiserror::Error;

use super::NoteFile;

/// Limits on the note files delivered to an acquisition gate during one import.
///
/// These limits bound delivered payloads. They do not bound allocations inside the existing
/// native note-file encoder. A zero limit refuses a nonempty acquisition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteAcquisitionLimits {
    /// Maximum number of input files in one import.
    pub max_notes: usize,
    /// Maximum encoded bytes delivered for one input file.
    pub max_note_file_bytes: usize,
    /// Maximum encoded bytes delivered across all input files.
    pub max_total_note_file_bytes: usize,
}

/// The native path that acquired a note-file candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteAcquisitionSource {
    /// A call to the public note import API.
    Import,
    /// An import from the note transport network.
    NoteTransport,
}

/// The amount of note information present in an acquisition candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoteAcquisitionVariant {
    /// Only the note ID is present. No private body is present.
    NoteId,
    /// Details and sync hints are present. Metadata and the note ID are absent.
    ExpectedNote,
    /// A complete note and an inclusion-proof candidate are present.
    Committed,
}

/// Immutable data delivered before an input note is imported.
///
/// The candidate is syntactically typed. Its inclusion proof and network state have not been
/// authenticated by this gate. A host must preserve a previously retained good generation when a
/// later candidate is refused. The callback must not re-enter the client, store, or WASM client.
pub struct NoteAcquisition {
    pub(crate) note_file_bytes: Vec<u8>,
    pub(crate) variant: NoteAcquisitionVariant,
    pub(crate) source: NoteAcquisitionSource,
    pub(crate) batch_index: usize,
    pub(crate) batch_count: usize,
    pub(crate) details_commitment: Option<NoteDetailsCommitment>,
    pub(crate) note_id: Option<NoteId>,
    pub(crate) nullifier: Option<Nullifier>,
    pub(crate) metadata: Option<NoteMetadata>,
    pub(crate) tag: Option<NoteTag>,
    pub(crate) after_block_num: Option<BlockNumber>,
}

impl NoteAcquisition {
    fn from_candidate(
        file: &NoteFile,
        note_file_bytes: Vec<u8>,
        source: NoteAcquisitionSource,
        batch_index: usize,
        batch_count: usize,
    ) -> Self {
        let (variant, details_commitment, note_id, nullifier, metadata, tag, after_block_num) =
            match file {
                NoteFile::NoteId(id) => {
                    (NoteAcquisitionVariant::NoteId, None, Some(*id), None, None, None, None)
                },
                NoteFile::ExpectedNote { details, sync_hint } => (
                    NoteAcquisitionVariant::ExpectedNote,
                    Some(details.commitment()),
                    None,
                    None,
                    None,
                    Some(sync_hint.tag()),
                    Some(sync_hint.after_block_num()),
                ),
                NoteFile::Committed { note, .. } => (
                    NoteAcquisitionVariant::Committed,
                    Some(note.details_commitment()),
                    Some(note.id()),
                    Some(note.nullifier()),
                    Some(*note.metadata()),
                    Some(note.metadata().tag()),
                    None,
                ),
            };
        Self {
            note_file_bytes,
            variant,
            source,
            batch_index,
            batch_count,
            details_commitment,
            note_id,
            nullifier,
            metadata,
            tag,
            after_block_num,
        }
    }

    /// Returns the canonical encoded note file.
    pub fn note_file_bytes(&self) -> &[u8] {
        &self.note_file_bytes
    }

    /// Returns the candidate's note-file variant.
    pub fn variant(&self) -> NoteAcquisitionVariant {
        self.variant
    }

    /// Returns the native acquisition path.
    pub fn source(&self) -> NoteAcquisitionSource {
        self.source
    }

    /// Returns the zero-based position in this import's input files.
    pub fn batch_index(&self) -> usize {
        self.batch_index
    }

    /// Returns the total number of input files in this import.
    pub fn batch_count(&self) -> usize {
        self.batch_count
    }

    /// Returns the details commitment, when private details are present.
    pub fn details_commitment(&self) -> Option<NoteDetailsCommitment> {
        self.details_commitment
    }

    /// Returns the note ID, when the file provides it.
    pub fn note_id(&self) -> Option<NoteId> {
        self.note_id
    }

    /// Returns the nullifier, when a complete note provides it.
    pub fn nullifier(&self) -> Option<Nullifier> {
        self.nullifier
    }

    /// Returns the supplied metadata. Expected-note files have no metadata.
    pub fn metadata(&self) -> Option<NoteMetadata> {
        self.metadata
    }

    /// Returns the actual tag supplied by metadata or a sync hint.
    pub fn tag(&self) -> Option<NoteTag> {
        self.tag
    }

    /// Returns the block hint supplied by an expected-note file.
    pub fn after_block_num(&self) -> Option<BlockNumber> {
        self.after_block_num
    }
}

/// A sanitized refusal from an acquisition gate.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum NoteAcquisitionError {
    /// A configured count or encoded-byte limit was exceeded.
    #[error("note acquisition limit exceeded")]
    LimitExceeded,
    /// An independent retention write, readback, or continuity check failed.
    #[error("note acquisition retention failed")]
    RetentionFailed,
    /// The host refused or cancelled the candidate for another reason.
    #[error("note acquisition rejected")]
    Rejected,
}

/// A fallible acknowledgement that must finish before note import writes begin.
///
/// The host must use independent storage. It must not call the client or re-enter a locked WASM
/// operation. A successful acknowledgement does not authenticate the candidate's inclusion proof.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait NoteAcquisitionGate: Send + Sync {
    /// Retains or refuses one candidate. Errors abort the import.
    async fn before_import(
        &self,
        acquisition: &NoteAcquisition,
    ) -> Result<(), NoteAcquisitionError>;
}

pub(crate) struct NoteAcquisitionConfig {
    pub(crate) gate: Arc<dyn NoteAcquisitionGate>,
    pub(crate) limits: NoteAcquisitionLimits,
}

impl NoteAcquisitionConfig {
    pub(crate) async fn acknowledge(
        &self,
        files: &[NoteFile],
        source: NoteAcquisitionSource,
    ) -> Result<(), NoteAcquisitionError> {
        if files.len() > self.limits.max_notes {
            return Err(NoteAcquisitionError::LimitExceeded);
        }
        let mut total_bytes = 0usize;
        for (index, file) in files.iter().enumerate() {
            let bytes = file.to_bytes();
            if bytes.len() > self.limits.max_note_file_bytes {
                return Err(NoteAcquisitionError::LimitExceeded);
            }
            total_bytes = total_bytes
                .checked_add(bytes.len())
                .filter(|total| *total <= self.limits.max_total_note_file_bytes)
                .ok_or(NoteAcquisitionError::LimitExceeded)?;
            let item = NoteAcquisition::from_candidate(file, bytes, source, index, files.len());
            self.gate.before_import(&item).await?;
        }
        Ok(())
    }
}

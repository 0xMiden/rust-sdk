use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::note::{
    Note,
    NoteAttachmentHeader,
    NoteAttachmentScheme,
    NoteAttachments,
    NoteDetails,
    NoteDetailsCommitment,
    NoteHeader,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteScript,
    NoteTag,
    NoteType,
    PartialNoteMetadata,
};
use miden_protocol::{MastForest, MastNodeId, Word};
use miden_tx::utils::serde::Deserializable;

use super::{MissingFieldHelper, RpcConversionError};
use crate::rpc::{RpcError, generated as proto};

impl From<NoteId> for proto::note::NoteId {
    fn from(value: NoteId) -> Self {
        proto::note::NoteId { id: Some(value.into()) }
    }
}

impl TryFrom<proto::note::NoteId> for NoteId {
    type Error = RpcConversionError;

    fn try_from(value: proto::note::NoteId) -> Result<Self, Self::Error> {
        let word =
            Word::try_from(value.id.ok_or(proto::note::NoteId::missing_field(stringify!(id)))?)?;
        Ok(Self::from_raw(word))
    }
}

fn note_type_from_proto(raw: i32) -> Result<NoteType, RpcConversionError> {
    let proto_note_type = proto::note::NoteType::try_from(raw)
        .map_err(|_| RpcConversionError::InvalidField(alloc::format!("note_type={raw}")))?;
    match proto_note_type {
        proto::note::NoteType::Public => Ok(NoteType::Public),
        proto::note::NoteType::Private => Ok(NoteType::Private),
        proto::note::NoteType::Unspecified => {
            Err(RpcConversionError::InvalidField("note_type=NOTE_TYPE_UNSPECIFIED".into()))
        },
    }
}

fn note_type_to_proto(note_type: NoteType) -> i32 {
    let proto_note_type = match note_type {
        NoteType::Public => proto::note::NoteType::Public,
        NoteType::Private => proto::note::NoteType::Private,
    };
    proto_note_type as i32
}

/// Decodes the `attachment_schemes` slice from a proto `NoteMetadata` into the fixed-size header
/// array expected by [`NoteMetadata::from_parts`]. Trailing absent slots may be omitted on the
/// wire; we pad with absent headers to reach the protocol's `NoteAttachments::MAX_COUNT`.
fn attachment_headers_from_proto(
    schemes: &[u32],
) -> Result<[NoteAttachmentHeader; NoteAttachments::MAX_COUNT], RpcConversionError> {
    if schemes.len() > NoteAttachments::MAX_COUNT {
        return Err(RpcConversionError::InvalidField(alloc::format!(
            "attachment_schemes length {} exceeds NoteAttachments::MAX_COUNT",
            schemes.len(),
        )));
    }
    let mut headers = [NoteAttachmentHeader::absent(); NoteAttachments::MAX_COUNT];
    for (slot, raw) in schemes.iter().enumerate() {
        if *raw == 0 {
            continue;
        }
        let raw_u16 = u16::try_from(*raw).map_err(|_| {
            RpcConversionError::InvalidField(alloc::format!(
                "attachment_schemes[{slot}]={raw} does not fit in u16",
            ))
        })?;
        let scheme = NoteAttachmentScheme::new(raw_u16).map_err(|err| {
            RpcConversionError::InvalidField(alloc::format!("attachment_schemes[{slot}]: {err}"))
        })?;
        headers[slot] = NoteAttachmentHeader::new(scheme);
    }
    Ok(headers)
}

fn attachment_schemes_to_proto(
    headers: &[NoteAttachmentHeader; NoteAttachments::MAX_COUNT],
) -> Vec<u32> {
    // Encode each header as the scheme value, with `0` meaning absent. Trailing absent slots
    // are stripped to match the wire convention.
    let mut encoded: Vec<u32> = headers
        .iter()
        .map(|h| h.scheme().map_or(0, |s| u32::from(s.as_u16())))
        .collect();
    while matches!(encoded.last(), Some(0)) {
        encoded.pop();
    }
    encoded
}

impl TryFrom<proto::note::NoteMetadata> for NoteMetadata {
    type Error = RpcConversionError;

    fn try_from(value: proto::note::NoteMetadata) -> Result<Self, Self::Error> {
        let partial_metadata: PartialNoteMetadata = (&value).try_into()?;
        let attachment_headers = attachment_headers_from_proto(&value.attachment_schemes)?;
        let attachments_commitment = value
            .attachments_commitment
            .ok_or_else(|| {
                proto::note::NoteMetadata::missing_field(stringify!(attachments_commitment))
            })?
            .try_into()?;

        Ok(NoteMetadata::from_parts(
            partial_metadata,
            attachment_headers,
            attachments_commitment,
        ))
    }
}

impl TryFrom<&proto::note::NoteMetadata> for PartialNoteMetadata {
    type Error = RpcConversionError;

    fn try_from(value: &proto::note::NoteMetadata) -> Result<Self, Self::Error> {
        let sender = value
            .sender
            .clone()
            .ok_or_else(|| proto::note::NoteMetadata::missing_field(stringify!(sender)))?
            .try_into()?;
        let note_type = note_type_from_proto(value.note_type)?;
        let tag = NoteTag::new(value.tag);

        Ok(PartialNoteMetadata::new(sender, note_type).with_tag(tag))
    }
}

impl From<NoteMetadata> for proto::note::NoteMetadata {
    fn from(value: NoteMetadata) -> Self {
        proto::note::NoteMetadata {
            sender: Some(value.sender().into()),
            note_type: note_type_to_proto(value.note_type()),
            tag: value.tag().as_u32(),
            attachment_schemes: attachment_schemes_to_proto(value.attachment_headers()),
            attachments_commitment: Some(value.attachments_commitment().into()),
        }
    }
}

impl TryFrom<proto::note::NoteHeader> for NoteHeader {
    type Error = RpcConversionError;

    fn try_from(value: proto::note::NoteHeader) -> Result<Self, Self::Error> {
        let details_commitment_word: Word = value
            .details_commitment
            .ok_or(proto::note::NoteHeader::missing_field(stringify!(details_commitment)))?
            .try_into()?;
        let metadata = value
            .metadata
            .ok_or(proto::note::NoteHeader::missing_field(stringify!(metadata)))?
            .try_into()?;
        Ok(NoteHeader::new(
            NoteDetailsCommitment::from_raw(details_commitment_word),
            metadata,
        ))
    }
}

impl TryFrom<proto::note::NoteInclusionInBlockProof> for NoteInclusionProof {
    type Error = RpcConversionError;

    fn try_from(value: proto::note::NoteInclusionInBlockProof) -> Result<Self, Self::Error> {
        Ok(NoteInclusionProof::new(
            value.block_num.into(),
            u16::try_from(value.note_index_in_block)
                .map_err(|_| RpcConversionError::InvalidField("NoteIndexInBlock".into()))?,
            value
                .inclusion_path
                .ok_or_else(|| {
                    proto::note::NoteInclusionInBlockProof::missing_field(stringify!(
                        inclusion_path
                    ))
                })?
                .try_into()?,
        )?)
    }
}

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

impl TryFrom<proto::rpc::sync_notes_response::NoteSyncBlock> for SyncNotesBlock {
    type Error = RpcError;

    fn try_from(
        block: proto::rpc::sync_notes_response::NoteSyncBlock,
    ) -> Result<Self, Self::Error> {
        let block_header = block
            .block_header
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(blocks.block_header)))?
            .try_into()?;

        let mmr_path = block
            .mmr_path
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(blocks.mmr_path)))?
            .try_into()?;

        let notes: BTreeMap<NoteId, CommittedNote> = block
            .notes
            .into_iter()
            .map(|n| {
                let note = CommittedNote::try_from(n)?;
                Ok((*note.note_id(), note))
            })
            .collect::<Result<_, RpcConversionError>>()?;

        Ok(SyncNotesBlock { block_header, mmr_path, notes })
    }
}

// SYNCED NOTE
// ================================================================================================

/// A block's worth of notes resolved by
/// [`NodeRpcClient::sync_notes_with_content`](crate::rpc::NodeRpcClient::sync_notes_with_content).
///
/// Unlike [`SyncNotesBlock`] (the raw `SyncNotes` response), each note here also carries the body
/// and attachment content fetched via `GetNotesById`, so a consumer never has to re-join two
/// parallel collections by note ID.
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
/// inclusion proof (always present, from `SyncNotes`), plus any body or attachment content
/// fetched via `GetNotesById`.
#[derive(Debug, Clone)]
pub struct SyncedNote {
    /// Note identity, metadata, and inclusion proof, as reported by `SyncNotes`.
    pub committed: CommittedNote,
    /// Body and/or attachment content resolved via `GetNotesById`; `None` if none was fetched
    /// (plain private notes, or public notes when bodies were not requested).
    pub content: Option<ResolvedNoteContent>,
}

impl SyncedNote {
    /// Pairs a sync record with the content resolved for it, checking that the content is
    /// consistent with the record's metadata:
    ///
    /// - The content variant must match the record's note type.
    /// - Resolved attachments must hash to the metadata's attachments commitment — the metadata is
    ///   what inclusion-proof verification later authenticates, so this binds the fetched bytes to
    ///   the on-chain note.
    /// - A note whose metadata advertises attachments must have resolved content: storing such a
    ///   note without its attachment content would leave it unconsumable with no retry path once
    ///   its expected-note tag is dropped.
    ///
    /// A rejection concerns a single note, not the response as a whole:
    /// [`NodeRpcClient::sync_notes_with_content`](crate::rpc::NodeRpcClient::sync_notes_with_content)
    /// skips the offending note with a warning instead of failing the sync, since content
    /// availability can be influenced by the note's creator.
    pub fn new(
        committed: CommittedNote,
        content: Option<ResolvedNoteContent>,
    ) -> Result<Self, RpcError> {
        match &content {
            Some(resolved) => {
                let expected_note_type = match resolved {
                    ResolvedNoteContent::Public { .. } => NoteType::Public,
                    ResolvedNoteContent::Private { .. } => NoteType::Private,
                };
                if committed.note_type() != expected_note_type {
                    return Err(RpcError::InvalidResponse(format!(
                        "content returned for note {} does not match the note's type",
                        committed.note_id()
                    )));
                }

                if resolved.attachments().to_commitment()
                    != committed.metadata().attachments_commitment()
                {
                    return Err(RpcError::InvalidResponse(format!(
                        "attachment content returned for note {} does not match the note's \
                         attachments commitment",
                        committed.note_id()
                    )));
                }
            },
            None => {
                if committed.has_attachments() {
                    return Err(RpcError::InvalidResponse(format!(
                        "note {} advertises attachments but the node did not return their content",
                        committed.note_id()
                    )));
                }
            },
        }

        Ok(Self { committed, content })
    }
}

/// Body and attachment content fetched for a note via `GetNotesById`.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum ResolvedNoteContent {
    /// Content fetched for a public note.
    Public {
        /// The public note body (recipient and assets, without metadata).
        details: NoteDetails,
        /// The note's attachment content. May be empty for a public note that carries none.
        attachments: NoteAttachments,
    },
    /// Content fetched for a private note. Private notes expose no on-chain body, so only their
    /// attachment content is resolved.
    Private {
        /// The note's attachment content.
        attachments: NoteAttachments,
    },
}

impl ResolvedNoteContent {
    /// Returns the attachment content fetched for the note.
    pub fn attachments(&self) -> &NoteAttachments {
        match self {
            Self::Public { attachments, .. } | Self::Private { attachments } => attachments,
        }
    }

    /// Consumes the content and returns the attachment content fetched for the note.
    pub fn into_attachments(self) -> NoteAttachments {
        match self {
            Self::Public { attachments, .. } | Self::Private { attachments } => attachments,
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
    /// Note metadata. Sync responses always carry the full [`NoteMetadata`] (header fields plus
    /// attachment scheme markers and the attachments commitment); attachment **content** is
    /// fetched separately via `GetNotesById`.
    metadata: NoteMetadata,
    /// Inclusion proof for the note in the block.
    inclusion_proof: NoteInclusionProof,
}

impl CommittedNote {
    pub fn new(
        note_id: NoteId,
        metadata: NoteMetadata,
        inclusion_proof: NoteInclusionProof,
    ) -> Self {
        Self { note_id, metadata, inclusion_proof }
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
    ///
    /// Sync records carry attachment scheme markers (not the attachment content), so a present
    /// scheme in any header slot indicates the note has attachments whose content must be fetched
    /// separately via `GetNotesById`.
    pub fn has_attachments(&self) -> bool {
        self.metadata
            .attachment_headers()
            .iter()
            .any(|header| header.scheme().is_some())
    }

    pub fn inclusion_proof(&self) -> &NoteInclusionProof {
        &self.inclusion_proof
    }

    /// Returns the number of the block in which the note was committed.
    pub fn block_num(&self) -> BlockNumber {
        self.inclusion_proof.location().block_num()
    }
}

impl TryFrom<proto::note::NoteSyncRecord> for CommittedNote {
    type Error = RpcConversionError;

    fn try_from(note: proto::note::NoteSyncRecord) -> Result<Self, Self::Error> {
        let proto_metadata = note
            .metadata
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(notes.metadata)))?;
        let metadata: NoteMetadata = proto_metadata.try_into()?;

        let proto_inclusion_proof = note.inclusion_proof.ok_or(
            proto::rpc::SyncNotesResponse::missing_field(stringify!(notes.inclusion_proof)),
        )?;

        let note_id: NoteId = proto_inclusion_proof
            .note_id
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(
                notes.inclusion_proof.note_id
            )))?
            .try_into()?;

        let inclusion_proof: NoteInclusionProof = proto_inclusion_proof.try_into()?;

        Ok(CommittedNote::new(note_id, metadata, inclusion_proof))
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

impl TryFrom<proto::note::CommittedNote> for FetchedNote {
    type Error = RpcConversionError;

    fn try_from(value: proto::note::CommittedNote) -> Result<Self, Self::Error> {
        let inclusion_proof = value.inclusion_proof.ok_or_else(|| {
            proto::note::CommittedNote::missing_field(stringify!(inclusion_proof))
        })?;

        let note_id: NoteId = inclusion_proof
            .note_id
            .ok_or_else(|| {
                proto::note::CommittedNote::missing_field(stringify!(inclusion_proof.note_id))
            })?
            .try_into()?;

        let inclusion_proof = NoteInclusionProof::try_from(inclusion_proof)?;

        let note = value
            .note
            .ok_or_else(|| proto::note::CommittedNote::missing_field(stringify!(note)))?;

        let proto_metadata = note
            .metadata
            .ok_or_else(|| proto::note::CommittedNote::missing_field(stringify!(note.metadata)))?;
        let metadata: NoteMetadata = proto_metadata.clone().try_into()?;
        let partial_metadata: PartialNoteMetadata = (&proto_metadata).try_into()?;

        let attachments = if note.attachments.is_empty() {
            NoteAttachments::empty()
        } else {
            NoteAttachments::read_from_bytes(&note.attachments)?
        };

        if let Some(detail_bytes) = note.details {
            let details = NoteDetails::read_from_bytes(&detail_bytes)?;
            let (assets, recipient) = details.into_parts();

            Ok(FetchedNote::Public(
                Note::with_attachments(assets, partial_metadata, recipient, attachments),
                inclusion_proof,
            ))
        } else {
            Ok(FetchedNote::Private(note_id, metadata, attachments, inclusion_proof))
        }
    }
}

// NOTE SCRIPT
// ================================================================================================

impl TryFrom<proto::note::NoteScript> for NoteScript {
    type Error = RpcConversionError;

    fn try_from(note_script: proto::note::NoteScript) -> Result<Self, Self::Error> {
        let mast_forest = MastForest::read_from_bytes(&note_script.mast)?;
        let entrypoint = MastNodeId::from_u32_safe(note_script.entrypoint, &mast_forest)?;
        Ok(NoteScript::from_parts(alloc::sync::Arc::new(mast_forest), entrypoint))
    }
}

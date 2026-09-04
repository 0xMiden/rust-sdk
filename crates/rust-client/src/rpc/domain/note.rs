use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec::Vec;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::SequentialCommit;
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::note::{
    Note,
    NoteAttachment,
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
use miden_protocol::{Felt, MastForest, MastNodeId, Word};
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

/// Aggregates individual attachment commitments into the note's attachments commitment.
///
/// The element layout mirrors [`NoteAttachments`]' own sequential commitment, so this yields the
/// same value as the full attachments would, which is what lets commitment-only records work.
struct AttachmentCommitments<'a>(&'a [Word]);

impl SequentialCommit for AttachmentCommitments<'_> {
    type Commitment = Word;

    fn to_elements(&self) -> Vec<Felt> {
        let mut elements = Vec::with_capacity(self.0.len() * miden_protocol::WORD_SIZE);
        for commitment in self.0 {
            elements.extend_from_slice(commitment.as_elements());
        }
        elements
    }
}

/// The attachments of a note as a `SyncNotes` record reports them. Both variants determine the
/// note's attachments commitment exactly, but only one carries the content.
#[derive(Debug)]
enum ReportedAttachments {
    /// Every attachment arrived verbatim, so the content is known and needs no fetching.
    Full(NoteAttachments),
    /// At least one attachment arrived as a commitment only. Holds one commitment per attachment,
    /// enough to rebuild the note's metadata but not its content.
    Commitments(Vec<Word>),
}

impl ReportedAttachments {
    /// Returns the note's attachments commitment.
    fn to_commitment(&self) -> Word {
        match self {
            Self::Full(attachments) => attachments.to_commitment(),
            Self::Commitments(commitments) => AttachmentCommitments(commitments).to_commitment(),
        }
    }

    /// Consumes the report and returns the content, when the record carried all of it.
    fn into_content(self) -> Option<NoteAttachments> {
        match self {
            Self::Full(attachments) => Some(attachments),
            Self::Commitments(_) => None,
        }
    }
}

/// The note metadata reconstructed from a `SyncNotes` record, together with what that record
/// reported about the note's attachments.
#[derive(Debug)]
struct SyncNoteMetadata {
    /// The note's full metadata, equal to what the on-chain note commits to.
    metadata: NoteMetadata,
    /// What the record reported about the note's attachments.
    attachments: ReportedAttachments,
}

impl TryFrom<proto::note::NoteSyncMetadata> for SyncNoteMetadata {
    type Error = RpcConversionError;

    fn try_from(value: proto::note::NoteSyncMetadata) -> Result<Self, Self::Error> {
        let sender = value
            .sender
            .ok_or_else(|| proto::note::NoteSyncMetadata::missing_field(stringify!(sender)))?
            .try_into()?;
        let note_type = note_type_from_proto(value.note_type)?;
        let tag = NoteTag::new(value.tag);
        let partial_metadata = PartialNoteMetadata::new(sender, note_type).with_tag(tag);

        if value.attachments.len() > NoteAttachments::MAX_COUNT {
            return Err(RpcConversionError::InvalidField(format!(
                "attachments length {} exceeds NoteAttachments::MAX_COUNT",
                value.attachments.len(),
            )));
        }

        let mut attachment_headers = [NoteAttachmentHeader::absent(); NoteAttachments::MAX_COUNT];
        let mut commitments = Vec::with_capacity(value.attachments.len());
        // Stays `Some` until an attachment arrives as a commitment only, since the content can be
        // rebuilt only as a whole set.
        let mut contents = Some(Vec::with_capacity(value.attachments.len()));

        for (slot, attachment) in value.attachments.into_iter().enumerate() {
            let raw_scheme = u16::try_from(attachment.scheme).map_err(|_| {
                RpcConversionError::InvalidField(format!(
                    "attachments[{slot}].scheme={} does not fit in u16",
                    attachment.scheme,
                ))
            })?;
            let scheme = NoteAttachmentScheme::new(raw_scheme).map_err(|err| {
                RpcConversionError::InvalidField(format!("attachments[{slot}].scheme: {err}"))
            })?;
            attachment_headers[slot] = NoteAttachmentHeader::new(scheme);

            let payload = attachment.payload.ok_or_else(|| {
                proto::note::NoteSyncAttachment::missing_field(stringify!(payload))
            })?;
            // An attachment that fits in a single word is sent verbatim, so it can be rebuilt in
            // full. A larger one is sent as a commitment to keep the sync response bounded. The
            // node may send a commitment even for a single-word one, so the choice is read off the
            // payload variant and never inferred from a word count.
            match payload {
                proto::note::note_sync_attachment::Payload::Value(value) => {
                    let attachment = NoteAttachment::with_word(scheme, Word::try_from(value)?);
                    commitments.push(attachment.to_commitment());
                    if let Some(contents) = contents.as_mut() {
                        contents.push(attachment);
                    }
                },
                proto::note::note_sync_attachment::Payload::Commitment(commitment) => {
                    commitments.push(Word::try_from(commitment)?);
                    contents = None;
                },
            }
        }

        let attachments = match contents {
            Some(contents) => {
                ReportedAttachments::Full(NoteAttachments::new(contents).map_err(|err| {
                    RpcConversionError::InvalidField(format!("attachments: {err}"))
                })?)
            },
            None => ReportedAttachments::Commitments(commitments),
        };

        Ok(SyncNoteMetadata {
            metadata: NoteMetadata::from_parts(
                partial_metadata,
                attachment_headers,
                attachments.to_commitment(),
            ),
            attachments,
        })
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
    /// Note identity, metadata, and inclusion proof, as reported by `SyncNotes`.
    pub committed: CommittedNote,
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

        Ok(Self { committed, details, attachments })
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
    /// Returns an error if the content does not hash to the metadata's attachments commitment.
    /// Such content would turn [`CommittedNote::needs_attachment_fetch`] off for a note whose real
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

    /// Returns the note's attachment content, `Some` when the reporting source
    /// carried every attachment verbatim.
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

impl TryFrom<proto::note::NoteSyncRecord> for CommittedNote {
    type Error = RpcConversionError;

    fn try_from(note: proto::note::NoteSyncRecord) -> Result<Self, Self::Error> {
        let proto_metadata = note
            .metadata
            .ok_or(proto::rpc::SyncNotesResponse::missing_field(stringify!(notes.metadata)))?;
        let SyncNoteMetadata { metadata, attachments } = proto_metadata.try_into()?;

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

        let committed = CommittedNote::new(note_id, metadata, inclusion_proof);

        match attachments.into_content() {
            Some(attachments) => committed.with_attachments(attachments),
            None => Ok(committed),
        }
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

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::account::{AccountIdVersion, AccountType, AssetCallbackFlag};
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::{NoteAssets, NoteRecipient, NoteStorage};
    use miden_standards::code_builder::CodeBuilder;

    use super::*;

    fn sender() -> AccountId {
        AccountId::dummy(
            [1; 15],
            AccountIdVersion::Version1,
            AccountType::Public,
            AssetCallbackFlag::Disabled,
        )
    }

    fn single_word_attachment(scheme: u16, word: u32) -> NoteAttachment {
        NoteAttachment::with_word(
            NoteAttachmentScheme::new(scheme).unwrap(),
            Word::from([word, word, word, word]),
        )
    }

    fn multi_word_attachment(scheme: u16) -> NoteAttachment {
        NoteAttachment::with_words(
            NoteAttachmentScheme::new(scheme).unwrap(),
            vec![Word::from([5u32, 6, 7, 8]), Word::from([9u32, 10, 11, 12])],
        )
        .unwrap()
    }

    /// Builds the sync record a node sends for `attachments`, then decodes it the way
    /// [`CommittedNote`] does.
    fn decode_sync_metadata(attachments: &NoteAttachments) -> SyncNoteMetadata {
        sync_metadata(sync_attachments(attachments)).try_into().unwrap()
    }

    fn bare_committed_note(metadata: NoteMetadata) -> CommittedNote {
        let path = SparseMerklePath::from_parts(0, Vec::new()).unwrap();
        let inclusion_proof =
            NoteInclusionProof::new(BlockNumber::GENESIS, 0, path).expect("index 0 is in range");

        CommittedNote::new(NoteId::from_raw(Word::empty()), metadata, inclusion_proof)
    }

    fn committed_note(decoded: SyncNoteMetadata) -> CommittedNote {
        let committed = bare_committed_note(decoded.metadata);

        match decoded.attachments.into_content() {
            Some(attachments) => committed.with_attachments(attachments).unwrap(),
            None => committed,
        }
    }

    /// Encodes attachments the way the node does in a sync response: single-word attachments carry
    /// their value, larger ones only their commitment.
    fn sync_attachments(attachments: &NoteAttachments) -> Vec<proto::note::NoteSyncAttachment> {
        attachments
            .iter()
            .map(|attachment| {
                let payload = if attachment.num_words() == 1 {
                    proto::note::note_sync_attachment::Payload::Value(
                        attachment.content().as_words()[0].into(),
                    )
                } else {
                    proto::note::note_sync_attachment::Payload::Commitment(
                        attachment.to_commitment().into(),
                    )
                };

                proto::note::NoteSyncAttachment {
                    scheme: u32::from(attachment.attachment_scheme().as_u16()),
                    payload: Some(payload),
                }
            })
            .collect()
    }

    fn sync_metadata(
        attachments: Vec<proto::note::NoteSyncAttachment>,
    ) -> proto::note::NoteSyncMetadata {
        proto::note::NoteSyncMetadata {
            sender: Some(sender().into()),
            note_type: note_type_to_proto(NoteType::Private),
            tag: 7,
            attachments,
        }
    }

    #[test]
    fn sync_metadata_reconstructs_metadata_with_mixed_attachments() {
        let attachments =
            NoteAttachments::new(vec![single_word_attachment(42, 1), multi_word_attachment(100)])
                .unwrap();

        let expected = NoteMetadata::new(
            PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
            &attachments,
        );

        assert_eq!(decode_sync_metadata(&attachments).metadata, expected);
    }

    #[test]
    fn sync_metadata_reconstructs_metadata_without_attachments() {
        let attachments = NoteAttachments::empty();
        let expected = NoteMetadata::new(
            PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
            &attachments,
        );

        let decoded: SyncNoteMetadata = sync_metadata(Vec::new()).try_into().unwrap();

        assert_eq!(decoded.metadata, expected);
    }

    /// A record whose every attachment fits in a single word describes the note's attachments in
    /// full, so no `GetNotesById` request is needed to obtain them.
    #[test]
    fn sync_metadata_reports_attachments_sent_verbatim() {
        let attachments = NoteAttachments::new(vec![
            single_word_attachment(42, 1),
            single_word_attachment(64, 2),
        ])
        .unwrap();

        let decoded = decode_sync_metadata(&attachments);

        let committed = committed_note(decoded);
        assert_eq!(committed.attachments(), Some(&attachments));
        assert!(!committed.needs_attachment_fetch());
    }

    /// A full set leaves no trailing slot absent, so it is the only case where the positional
    /// header fill writes every slot.
    #[test]
    fn sync_metadata_reconstructs_a_full_attachment_set() {
        let attachments = NoteAttachments::new(
            (0..NoteAttachments::MAX_COUNT)
                .map(|i| {
                    let scheme = u16::try_from(i).unwrap() + 42;
                    single_word_attachment(scheme, u32::try_from(i).unwrap() + 1)
                })
                .collect(),
        )
        .unwrap();

        let expected = NoteMetadata::new(
            PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
            &attachments,
        );
        let decoded = decode_sync_metadata(&attachments);

        assert_eq!(decoded.metadata, expected);
        let committed = committed_note(decoded);
        assert_eq!(committed.attachments(), Some(&attachments));
        assert!(!committed.needs_attachment_fetch());
    }

    /// A note with no attachments has nothing to fetch, and its (empty) attachments are known.
    #[test]
    fn sync_metadata_reports_empty_attachments() {
        let decoded: SyncNoteMetadata = sync_metadata(Vec::new()).try_into().unwrap();

        let committed = committed_note(decoded);
        assert_eq!(committed.attachments(), Some(&NoteAttachments::empty()));
        assert!(!committed.needs_attachment_fetch());
    }

    /// One attachment sent as a commitment withholds the whole set, since the attachments can only
    /// be rebuilt as a whole.
    #[test]
    fn sync_metadata_withholds_partially_reported_attachments() {
        let attachments =
            NoteAttachments::new(vec![single_word_attachment(42, 1), multi_word_attachment(100)])
                .unwrap();

        let decoded = decode_sync_metadata(&attachments);

        assert!(matches!(decoded.attachments, ReportedAttachments::Commitments(_)));
        assert!(committed_note(decoded).needs_attachment_fetch());
    }

    /// The node may send a commitment even for a single-word attachment, so availability follows
    /// the payload variant alone, never a word count.
    #[test]
    fn sync_metadata_withholds_single_word_attachment_sent_as_commitment() {
        let attachment = single_word_attachment(42, 1);
        let proto_attachments = vec![proto::note::NoteSyncAttachment {
            scheme: u32::from(attachment.attachment_scheme().as_u16()),
            payload: Some(proto::note::note_sync_attachment::Payload::Commitment(
                attachment.to_commitment().into(),
            )),
        }];
        let attachments = NoteAttachments::new(vec![attachment]).unwrap();

        let decoded: SyncNoteMetadata = sync_metadata(proto_attachments).try_into().unwrap();

        // The metadata is still reconstructed exactly, only the content is missing.
        assert_eq!(
            decoded.metadata,
            NoteMetadata::new(
                PartialNoteMetadata::new(sender(), NoteType::Private).with_tag(NoteTag::new(7)),
                &attachments,
            )
        );
        assert!(matches!(decoded.attachments, ReportedAttachments::Commitments(_)));
        assert!(committed_note(decoded).needs_attachment_fetch());
    }

    #[test]
    fn sync_metadata_rejects_too_many_attachments() {
        let attachment = proto::note::NoteSyncAttachment {
            scheme: 42,
            payload: Some(proto::note::note_sync_attachment::Payload::Value(Word::empty().into())),
        };
        let attachments = vec![attachment; NoteAttachments::MAX_COUNT + 1];

        let err = SyncNoteMetadata::try_from(sync_metadata(attachments)).unwrap_err();

        assert!(matches!(err, RpcConversionError::InvalidField(_)), "got {err:?}");
    }

    #[test]
    fn sync_metadata_rejects_reserved_absent_scheme() {
        let attachments = vec![proto::note::NoteSyncAttachment {
            scheme: 0,
            payload: Some(proto::note::note_sync_attachment::Payload::Value(Word::empty().into())),
        }];

        let err = SyncNoteMetadata::try_from(sync_metadata(attachments)).unwrap_err();

        assert!(matches!(err, RpcConversionError::InvalidField(_)), "got {err:?}");
    }

    #[test]
    fn sync_metadata_rejects_missing_attachment_payload() {
        let attachments = vec![proto::note::NoteSyncAttachment { scheme: 42, payload: None }];

        let err = SyncNoteMetadata::try_from(sync_metadata(attachments)).unwrap_err();

        assert!(
            matches!(err, RpcConversionError::MissingFieldInProtobufRepresentation { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn synced_note_rejects_a_body_for_a_private_note() {
        let decoded: SyncNoteMetadata = sync_metadata(Vec::new()).try_into().unwrap();
        let committed = committed_note(decoded);

        let note_script = CodeBuilder::new()
            .compile_note_script("@note_script\npub proc main\n    nop\nend")
            .unwrap();
        let recipient =
            NoteRecipient::new(Word::empty(), note_script, NoteStorage::new(vec![]).unwrap());
        let details = NoteDetails::new(NoteAssets::new(vec![]).unwrap(), recipient);

        let err = SyncedNote::new(committed, Some(details), NoteAttachments::empty()).unwrap_err();

        assert!(matches!(err, RpcError::InvalidResponse(_)), "got {err:?}");
    }

    /// A note advertising attachments whose content never arrived is rejected: empty attachments
    /// hash to a different commitment than the note's.
    #[test]
    fn synced_note_rejects_unresolved_attachments() {
        let attachments = NoteAttachments::new(vec![multi_word_attachment(100)]).unwrap();
        let committed = committed_note(decode_sync_metadata(&attachments));

        let err = SyncedNote::new(committed, None, NoteAttachments::empty()).unwrap_err();

        assert!(matches!(err, RpcError::InvalidResponse(_)), "got {err:?}");
    }
}

use alloc::string::ToString;
use core::fmt::{self, Display};

use miden_protocol::Word;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{
    Note,
    NoteAssets,
    NoteAttachments,
    NoteDetails,
    NoteDetailsCommitment,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteRecipient,
    Nullifier,
    PartialNote,
};
use miden_protocol::transaction::RawOutputNote;
use miden_standards::note::{NoteFile, NoteSyncHint};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use super::NoteRecordError;

// OUTPUT NOTE RECORD
// ================================================================================================

/// Represents a Note which was the result of executing some transaction of which the Store can
/// keep track and retrieve.
///
/// An [`OutputNoteRecord`] contains all the information of a [`Note`] while it allows for not
/// knowing the recipient details (nullifier, script, inputs and serial number).
///
/// It is also possible to convert [`Note`] into [`OutputNoteRecord`] with the state
/// [`OutputNoteState::ExpectedFull`].
#[derive(Clone, Debug, PartialEq)]
pub struct OutputNoteRecord {
    /// Assets contained in the note.
    assets: NoteAssets,
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information.
    metadata: NoteMetadata,
    /// A commitment to the note's serial number, script and inputs.
    recipient_digest: Word,
    /// The state of the note, with specific fields for each one.
    state: OutputNoteState,
    /// The expected block height at which the note should be included in the chain.
    expected_height: BlockNumber,
    /// The note's attachments. Required to reconstruct a [`Note`] whose commitment matches the
    /// on-chain note, since the attachments contribute to the note metadata commitment. Empty when
    /// the note's full details have not been fetched yet (e.g. expected notes).
    attachments: NoteAttachments,
}

impl OutputNoteRecord {
    pub fn new(
        recipient_digest: Word,
        assets: NoteAssets,
        metadata: NoteMetadata,
        state: OutputNoteState,
        expected_height: BlockNumber,
        attachments: NoteAttachments,
    ) -> OutputNoteRecord {
        OutputNoteRecord {
            assets,
            metadata,
            recipient_digest,
            state,
            expected_height,
            attachments,
        }
    }

    pub fn id(&self) -> NoteId {
        NoteId::new(self.details_commitment(), &self.metadata)
    }

    /// Returns the commitment to the note's details (recipient + assets), independent of
    /// note metadata.
    pub fn details_commitment(&self) -> NoteDetailsCommitment {
        NoteDetailsCommitment::from_raw_commitments(self.recipient_digest, self.assets.commitment())
    }

    pub fn recipient_digest(&self) -> Word {
        self.recipient_digest
    }

    pub fn assets(&self) -> &NoteAssets {
        &self.assets
    }

    pub fn state(&self) -> &OutputNoteState {
        &self.state
    }

    pub fn metadata(&self) -> &NoteMetadata {
        &self.metadata
    }

    pub fn attachments(&self) -> &NoteAttachments {
        &self.attachments
    }

    pub fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        self.state.inclusion_proof()
    }

    pub fn recipient(&self) -> Option<&NoteRecipient> {
        self.state.recipient()
    }

    pub fn nullifier(&self) -> Option<Nullifier> {
        let recipient = self.recipient()?;
        let details = NoteDetails::new(self.assets.clone(), recipient.clone());
        Some(Nullifier::from_details_and_metadata(&details, &self.metadata))
    }

    pub fn expected_height(&self) -> BlockNumber {
        self.expected_height
    }

    /// Returns true if the note has been nullified on chain.
    pub fn is_consumed(&self) -> bool {
        matches!(self.state, OutputNoteState::Consumed { .. })
    }

    /// Returns true if the note is in a committed state (i.e. it has a inclusion proof but isn't
    /// consumed) regardless of whether it is full or partial.
    pub fn is_committed(&self) -> bool {
        matches!(
            self.state,
            OutputNoteState::CommittedFull { .. } | OutputNoteState::CommittedPartial { .. }
        )
    }

    // TRANSITIONS
    // --------------------------------------------------------------------------------------------

    pub(crate) fn inclusion_proof_received(
        &mut self,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<bool, NoteRecordError> {
        let new_state = self.state.inclusion_proof_received(inclusion_proof)?;
        if let Some(new_state) = new_state {
            self.state = new_state;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub(crate) fn nullifier_received(
        &mut self,
        nullifier: Nullifier,
        block_height: BlockNumber,
    ) -> Result<bool, NoteRecordError> {
        if let Some(note_nullifier) = self.nullifier()
            && note_nullifier != nullifier
        {
            return Err(NoteRecordError::StateTransitionError(
                "Nullifier does not match the expected value".to_string(),
            ));
        }

        let new_state = self.state.nullifier_received(block_height)?;
        if let Some(new_state) = new_state {
            self.state = new_state;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// CONVERSIONS
// ================================================================================================

// TODO: Improve conversions by implementing into_parts()
impl OutputNoteRecord {
    pub fn from_full_note(note: Note, expected_height: BlockNumber) -> Self {
        let header = *note.header();
        let attachments = note.attachments().clone();
        let (assets, recipient) = NoteDetails::from(note).into_parts();
        OutputNoteRecord {
            recipient_digest: recipient.digest(),
            assets,
            metadata: *header.metadata(),
            state: OutputNoteState::ExpectedFull { recipient },
            expected_height,
            attachments,
        }
    }

    pub fn from_partial_note(partial_note: &PartialNote, expected_height: BlockNumber) -> Self {
        OutputNoteRecord {
            recipient_digest: partial_note.recipient_digest(),
            assets: partial_note.assets().clone(),
            metadata: *partial_note.metadata(),
            state: OutputNoteState::ExpectedPartial,
            expected_height,
            attachments: partial_note.attachments().clone(),
        }
    }

    /// [`RawOutputNote`] can always be turned into an [`OutputNoteRecord`] when they're either
    /// [`RawOutputNote::Full`] or [`RawOutputNote::Partial`].
    pub fn try_from_output_note(
        output_note: RawOutputNote,
        expected_height: BlockNumber,
    ) -> Result<Self, NoteRecordError> {
        match output_note {
            RawOutputNote::Full(note) => Ok(Self::from_full_note(note, expected_height)),
            RawOutputNote::Partial(partial_note) => {
                Ok(Self::from_partial_note(&partial_note, expected_height))
            },
        }
    }
}

impl TryFrom<OutputNoteRecord> for NoteDetails {
    type Error = NoteRecordError;
    fn try_from(value: OutputNoteRecord) -> Result<Self, Self::Error> {
        match value.recipient() {
            Some(recipient) => Ok(NoteDetails::new(value.assets.clone(), recipient.clone())),
            None => Err(NoteRecordError::ConversionError(
                "Output Note Record does not contain details".to_string(),
            )),
        }
    }
}

impl TryFrom<OutputNoteRecord> for Note {
    type Error = NoteRecordError;

    fn try_from(value: OutputNoteRecord) -> Result<Self, Self::Error> {
        match value.recipient() {
            Some(recipient) => {
                let note = Note::with_attachments(
                    value.assets.clone(),
                    *value.metadata.partial_metadata(),
                    recipient.clone(),
                    value.attachments.clone(),
                );
                Ok(note)
            },
            None => Err(NoteRecordError::ConversionError(
                "Output Note Record does not contain details".to_string(),
            )),
        }
    }
}

/// Variants of [`NoteFile`] that can be exported from an [`OutputNoteRecord`].
pub enum NoteExportType {
    /// Export only the note ID.
    NoteId,
    /// Export the partial note with minimal details.
    NoteDetails,
    /// Export the full note with inclusion proof.
    NoteWithProof,
}

impl OutputNoteRecord {
    /// Converts the output note record into a note file for exporting. The export type is used to
    /// determine the variant of [`NoteFile`] to be created.
    ///
    /// # Errors
    ///
    /// Will return an error if there isn't enough information to create the requested [`NoteFile`]
    /// variant.
    pub fn into_note_file(self, export_type: &NoteExportType) -> Result<NoteFile, NoteRecordError> {
        match export_type {
            NoteExportType::NoteId => Ok(NoteFile::NoteId(self.id())),
            NoteExportType::NoteDetails => {
                let after_block_num = self.expected_height();
                let tag = self.metadata().tag();

                Ok(NoteFile::ExpectedNote {
                    details: self.try_into()?,
                    sync_hint: NoteSyncHint::new(after_block_num, tag),
                })
            },
            NoteExportType::NoteWithProof => {
                let proof = self
                    .inclusion_proof()
                    .ok_or(NoteRecordError::ConversionError(
                        "Note record does not contain an inclusion proof".to_string(),
                    ))?
                    .clone();

                Ok(NoteFile::Committed { note: self.try_into()?, proof })
            },
        }
    }
}

// OUTPUT NOTE STATE
// ================================================================================================

/// Possible states for a single output note. They describe the note's state and dictate its
/// lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputNoteState {
    /// Note without known recipient is expected to be committed on chain.
    ExpectedPartial,
    /// Note with known recipient is expected to be committed on chain.
    ExpectedFull {
        /// Details needed consume the note.
        recipient: NoteRecipient,
    },
    /// Note without known recipient has been committed on chain, and can be consumed in a
    /// transaction.
    CommittedPartial {
        /// Inclusion proof for the note inside the chain block.
        inclusion_proof: NoteInclusionProof,
    },
    /// Note with known recipient has been committed on chain, and can be consumed in a
    /// transaction.
    CommittedFull {
        /// Details needed to consume the note.
        recipient: NoteRecipient,
        /// Inclusion proof for the note inside the chain block.
        inclusion_proof: NoteInclusionProof,
    },
    /// Note has been nullified on chain.
    Consumed {
        /// Block height at which the note was consumed.
        block_height: BlockNumber,
        /// Details needed to consume the note.
        recipient: NoteRecipient,
    },
}

impl OutputNoteState {
    pub const STATE_EXPECTED_PARTIAL: u8 = 0;
    pub const STATE_EXPECTED_FULL: u8 = 1;
    pub const STATE_COMMITTED_PARTIAL: u8 = 2;
    pub const STATE_COMMITTED_FULL: u8 = 3;
    pub const STATE_CONSUMED: u8 = 4;

    /// Returns a unique identifier for each note state.
    pub fn discriminant(&self) -> u8 {
        match self {
            OutputNoteState::ExpectedPartial => Self::STATE_EXPECTED_PARTIAL,
            OutputNoteState::ExpectedFull { .. } => Self::STATE_EXPECTED_FULL,
            OutputNoteState::CommittedPartial { .. } => Self::STATE_COMMITTED_PARTIAL,
            OutputNoteState::CommittedFull { .. } => Self::STATE_COMMITTED_FULL,
            OutputNoteState::Consumed { .. } => Self::STATE_CONSUMED,
        }
    }

    pub fn recipient(&self) -> Option<&NoteRecipient> {
        match self {
            OutputNoteState::ExpectedFull { recipient, .. }
            | OutputNoteState::CommittedFull { recipient, .. }
            | OutputNoteState::Consumed { recipient, .. } => Some(recipient),
            _ => None,
        }
    }

    pub fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        match self {
            OutputNoteState::CommittedPartial { inclusion_proof, .. }
            | OutputNoteState::CommittedFull { inclusion_proof, .. } => Some(inclusion_proof),
            _ => None,
        }
    }

    pub fn inclusion_proof_received(
        &self,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<Option<OutputNoteState>, NoteRecordError> {
        match self {
            OutputNoteState::ExpectedPartial => {
                Ok(Some(OutputNoteState::CommittedPartial { inclusion_proof }))
            },
            OutputNoteState::ExpectedFull { recipient, .. } => {
                Ok(Some(OutputNoteState::CommittedFull {
                    recipient: recipient.clone(),
                    inclusion_proof,
                }))
            },
            OutputNoteState::CommittedPartial { inclusion_proof: prev_inclusion_proof }
            | OutputNoteState::CommittedFull {
                inclusion_proof: prev_inclusion_proof, ..
            } => {
                if prev_inclusion_proof == &inclusion_proof {
                    Ok(None)
                } else {
                    Err(NoteRecordError::StateTransitionError(
                        "Cannot receive different inclusion proof for committed note".to_string(),
                    ))
                }
            },
            OutputNoteState::Consumed { .. } => Ok(None),
        }
    }

    pub fn nullifier_received(
        &self,
        block_height: BlockNumber,
    ) -> Result<Option<OutputNoteState>, NoteRecordError> {
        match self {
            OutputNoteState::Consumed { .. } => Ok(None),
            OutputNoteState::ExpectedFull { recipient, .. }
            | OutputNoteState::CommittedFull { recipient, .. } => {
                Ok(Some(OutputNoteState::Consumed {
                    block_height,
                    recipient: recipient.clone(),
                }))
            },
            OutputNoteState::ExpectedPartial | OutputNoteState::CommittedPartial { .. } => {
                Err(NoteRecordError::InvalidStateTransition(
                    "Cannot nullify note without recipient".to_string(),
                ))
            },
        }
    }
}

impl Serializable for OutputNoteRecord {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.recipient_digest.write_into(target);
        self.assets.write_into(target);
        self.metadata.write_into(target);
        self.state.write_into(target);
        self.expected_height.write_into(target);
        self.attachments.write_into(target);
    }
}

impl Deserializable for OutputNoteRecord {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let recipient_digest = Word::read_from(source)?;
        let assets = NoteAssets::read_from(source)?;
        let metadata = NoteMetadata::read_from(source)?;
        let state = OutputNoteState::read_from(source)?;
        let expected_height = BlockNumber::read_from(source)?;
        let attachments = NoteAttachments::read_from(source)?;

        Ok(Self {
            assets,
            metadata,
            recipient_digest,
            state,
            expected_height,
            attachments,
        })
    }
}

impl Serializable for OutputNoteState {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.discriminant().write_into(target);
        match self {
            OutputNoteState::ExpectedPartial => {},
            OutputNoteState::ExpectedFull { recipient } => {
                recipient.write_into(target);
            },
            OutputNoteState::CommittedPartial { inclusion_proof } => {
                inclusion_proof.write_into(target);
            },
            OutputNoteState::CommittedFull { recipient, inclusion_proof } => {
                recipient.write_into(target);
                inclusion_proof.write_into(target);
            },
            OutputNoteState::Consumed { block_height, recipient } => {
                block_height.write_into(target);
                recipient.write_into(target);
            },
        }
    }
}

impl Deserializable for OutputNoteState {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let state = source.read_u8()?;
        match state {
            Self::STATE_EXPECTED_PARTIAL => Ok(OutputNoteState::ExpectedPartial),
            Self::STATE_EXPECTED_FULL => {
                let recipient = NoteRecipient::read_from(source)?;
                Ok(OutputNoteState::ExpectedFull { recipient })
            },
            Self::STATE_COMMITTED_PARTIAL => {
                let inclusion_proof = NoteInclusionProof::read_from(source)?;
                Ok(OutputNoteState::CommittedPartial { inclusion_proof })
            },
            Self::STATE_COMMITTED_FULL => {
                let recipient = NoteRecipient::read_from(source)?;
                let inclusion_proof = NoteInclusionProof::read_from(source)?;
                Ok(OutputNoteState::CommittedFull { recipient, inclusion_proof })
            },
            Self::STATE_CONSUMED => {
                let block_height = BlockNumber::read_from(source)?;
                let recipient = NoteRecipient::read_from(source)?;
                Ok(OutputNoteState::Consumed { block_height, recipient })
            },
            _ => Err(DeserializationError::InvalidValue("OutputNoteState".to_string())),
        }
    }
}

impl Display for OutputNoteState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OutputNoteState::ExpectedPartial => {
                write!(f, "Expected Partial")
            },
            OutputNoteState::ExpectedFull { .. } => {
                write!(f, "Expected Full")
            },
            OutputNoteState::CommittedPartial { inclusion_proof } => {
                write!(
                    f,
                    "Committed Partial (at block height {})",
                    inclusion_proof.location().block_num()
                )
            },
            OutputNoteState::CommittedFull { inclusion_proof, .. } => {
                write!(
                    f,
                    "Committed Full (at block height {})",
                    inclusion_proof.location().block_num()
                )
            },
            OutputNoteState::Consumed { block_height, .. } => {
                write!(f, "Consumed (at block height {block_height})")
            },
        }
    }
}

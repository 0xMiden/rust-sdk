use alloc::string::ToString;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{
    Note,
    NoteAssets,
    NoteAttachments,
    NoteDetails,
    NoteDetailsCommitment,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    Nullifier,
};
use miden_protocol::transaction::{InputNote, TransactionId};
use miden_protocol::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use super::NoteRecordError;

mod states;
pub use states::{
    CommittedNoteState,
    ConsumedAuthenticatedLocalNoteState,
    ConsumedExternalNoteState,
    ConsumedUnauthenticatedLocalNoteState,
    ExpectedNoteState,
    InputNoteState,
    InvalidNoteState,
    NoteSubmissionData,
    ProcessingAuthenticatedNoteState,
    ProcessingUnauthenticatedNoteState,
    UnverifiedNoteState,
};

// INPUT NOTE RECORD
// ================================================================================================

/// Represents a Note of which the Store can keep track and retrieve.
///
/// An [`InputNoteRecord`] contains all the information of a [`NoteDetails`], in addition of
/// specific information about the note state.
///
/// Once a proof is received, the [`InputNoteRecord`] can be transformed into an [`InputNote`] and
/// used as input for transactions.
/// It is also possible to convert [`Note`] and [`InputNote`] into [`InputNoteRecord`] (we fill the
/// `metadata` and `inclusion_proof` fields if possible).
///
/// Notes can also be consumed as unauthenticated notes, where their existence is verified by
/// the network.
#[derive(Clone, Debug, PartialEq)]
pub struct InputNoteRecord {
    /// Details of a note consisting of assets, script, inputs, and a serial number.
    details: NoteDetails,
    /// The note's attachments. Required to reconstruct a [`Note`] whose commitment matches the
    /// on-chain note, since the attachments contribute to the note metadata commitment. Empty when
    /// the note's full details have not been fetched yet (e.g. expected notes).
    attachments: NoteAttachments,
    /// The timestamp at which the note was created. If it's not known, it will be None.
    created_at: Option<u64>,
    /// The state of the note, with specific fields for each one.
    state: InputNoteState,
}

impl InputNoteRecord {
    pub fn new(
        details: NoteDetails,
        attachments: NoteAttachments,
        created_at: Option<u64>,
        state: InputNoteState,
    ) -> InputNoteRecord {
        InputNoteRecord { details, attachments, created_at, state }
    }

    // PUBLIC ACCESSORS
    // ================================================================================================

    /// Returns the input note ID, computed by combining the details commitment with the
    /// note metadata. Returns `None` when the current state has no metadata (e.g. an
    /// expected note imported from bare `NoteFile::NoteDetails`). Use
    /// [`Self::details_commitment`] when a stable identifier is needed in those cases.
    pub fn id(&self) -> Option<NoteId> {
        let metadata = self.metadata()?;
        Some(NoteId::new(self.details.commitment(), metadata))
    }

    /// Returns the commitment to the note's details (recipient + assets), independent of
    /// note metadata.
    pub fn details_commitment(&self) -> NoteDetailsCommitment {
        self.details.commitment()
    }

    /// Returns the note's recipient.
    pub fn recipient(&self) -> Word {
        self.details.recipient().digest()
    }

    /// Returns the note's commitment, if the record contains the [`NoteMetadata`].
    pub fn commitment(&self) -> Option<Word> {
        self.metadata()
            .map(|metadata| NoteId::new(self.details.commitment(), metadata).as_word())
    }

    /// Returns the note's assets.
    pub fn assets(&self) -> &NoteAssets {
        self.details.assets()
    }

    /// Returns the note's attachments.
    pub fn attachments(&self) -> &NoteAttachments {
        &self.attachments
    }

    /// Sets the note's attachments, returning `true` if the record changed.
    ///
    /// Attachments are a top-level field of the record, independent of the [`InputNoteState`]
    /// machine. They are populated during sync once fetched from the node, since they are
    /// required to reconstruct the note's ID for consumption.
    pub(crate) fn attachments_received(&mut self, attachments: NoteAttachments) -> bool {
        if self.attachments == attachments {
            return false;
        }
        self.attachments = attachments;
        true
    }

    /// Returns the timestamp in which the note record was created, if available.
    pub fn created_at(&self) -> Option<u64> {
        self.created_at
    }

    /// Returns the current note state.
    pub fn state(&self) -> &InputNoteState {
        &self.state
    }

    /// Returns the note metadata, which will be available depending on the note's current state.
    pub fn metadata(&self) -> Option<&NoteMetadata> {
        self.state.metadata()
    }

    /// Returns the note nullifier, if the record contains the [`NoteMetadata`].
    pub fn nullifier(&self) -> Option<Nullifier> {
        let metadata = self.metadata()?;
        Some(Nullifier::from_details_and_metadata(&self.details, metadata))
    }

    /// Returns the inclusion proof for the note.
    pub fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        self.state.inclusion_proof()
    }

    /// Returns the note's details.
    pub fn details(&self) -> &NoteDetails {
        &self.details
    }

    /// If the note was consumed locally, it returns the corresponding transaction ID.
    /// Otherwise, returns `None`.
    pub fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        self.state.consumer_transaction_id()
    }

    /// Returns the account ID that consumed this note, if available.
    ///
    /// This is available for notes in processing, consumed-local, or consumed-external
    /// states. For externally consumed notes, the account is only known when it is tracked
    /// by this client. Returns `None` for notes that haven't been submitted for consumption,
    /// invalid notes, or externally consumed notes where the consuming account is unknown.
    pub fn consumer_account(&self) -> Option<AccountId> {
        match &self.state {
            InputNoteState::ProcessingAuthenticated(s) => Some(s.submission_data.consumer_account),
            InputNoteState::ProcessingUnauthenticated(s) => {
                Some(s.submission_data.consumer_account)
            },
            InputNoteState::ConsumedAuthenticatedLocal(s) => {
                Some(s.submission_data.consumer_account)
            },
            InputNoteState::ConsumedUnauthenticatedLocal(s) => {
                Some(s.submission_data.consumer_account)
            },
            InputNoteState::ConsumedExternal(s) => s.consumer_account,
            _ => None,
        }
    }

    /// Returns true if the note is authenticated, meaning that it has the necessary inclusion
    /// proof and block header information to be considered valid.
    pub fn is_authenticated(&self) -> bool {
        matches!(
            self.state,
            InputNoteState::Committed { .. }
                | InputNoteState::ProcessingAuthenticated { .. }
                | InputNoteState::ConsumedAuthenticatedLocal { .. }
        )
    }

    /// Returns true if the note has been nullified on chain.
    pub fn is_consumed(&self) -> bool {
        matches!(
            self.state,
            InputNoteState::ConsumedExternal { .. }
                | InputNoteState::ConsumedAuthenticatedLocal { .. }
                | InputNoteState::ConsumedUnauthenticatedLocal { .. }
        )
    }

    /// Returns true if the note is currently being processed by a local transaction.
    pub fn is_processing(&self) -> bool {
        matches!(
            self.state,
            InputNoteState::ProcessingAuthenticated { .. }
                | InputNoteState::ProcessingUnauthenticated { .. }
        )
    }

    /// Returns true if the note is in a committed state (i.e. it has a valid inclusion proof but
    /// isn't consumed or being processed).
    pub fn is_committed(&self) -> bool {
        matches!(self.state, InputNoteState::Committed { .. })
    }

    /// Returns true while the note's on-chain inclusion is still unsettled (`Expected` or
    /// `Unverified`), i.e. while note sync is the mechanism that can advance this record.
    pub fn is_inclusion_pending(&self) -> bool {
        matches!(self.state, InputNoteState::Expected { .. } | InputNoteState::Unverified { .. })
    }

    /// Sets the consumed transaction order on the inner note state. No-op if the note is not in
    /// a consumed state.
    pub fn set_consumed_tx_order(&mut self, order: Option<u32>) {
        self.state.set_consumed_tx_order(order);
    }

    // TRANSITIONS
    // ================================================================================================

    /// Modifies the state of the note record to reflect that the it has received an inclusion
    /// proof. It is assumed to be unverified until the block header information is received.
    /// Returns `true` if the state was changed.
    pub(crate) fn inclusion_proof_received(
        &mut self,
        inclusion_proof: NoteInclusionProof,
        metadata: NoteMetadata,
    ) -> Result<bool, NoteRecordError> {
        let new_state = self.state.inclusion_proof_received(inclusion_proof, metadata)?;
        if let Some(new_state) = new_state {
            self.state = new_state;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Modifies the state of the note record to reflect that the it has received a block header.
    /// This will mark the note as verified or invalid, depending on the block header
    /// information and inclusion proof. Returns `true` if the state was changed.
    pub(crate) fn block_header_received(
        &mut self,
        block_header: &BlockHeader,
    ) -> Result<bool, NoteRecordError> {
        let note_id = self
            .id()
            .expect("block_header_received is only called after metadata is populated");
        let new_state = self.state.block_header_received(note_id, block_header)?;
        if let Some(new_state) = new_state {
            self.state = new_state;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Modifies the state of the note record to reflect that the note has been consumed by a
    /// transaction not submitted by this client. Returns `true` if the state was changed.
    ///
    /// `consumer_account` is `Some` when the consuming account is tracked by this client
    /// (derived from `sync_transactions` data). It is `None` for untracked accounts.
    ///
    /// Errors:
    /// - If the nullifier doesn't match the expected value.
    pub(crate) fn consumed_externally(
        &mut self,
        nullifier: Nullifier,
        nullifier_block_height: BlockNumber,
        consumer_account: Option<AccountId>,
    ) -> Result<bool, NoteRecordError> {
        if self.nullifier() != Some(nullifier) {
            return Err(NoteRecordError::StateTransitionError(
                "Nullifier does not match the expected value".to_string(),
            ));
        }

        let new_state = self.state.consumed_externally(nullifier_block_height, consumer_account)?;
        if let Some(new_state) = new_state {
            self.state = new_state;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Modifies the state of the note record to reflect that the client began processing the note
    /// to be consumed. Returns `true` if the state was changed.
    pub(crate) fn consumed_locally(
        &mut self,
        consumer_account: AccountId,
        consumer_transaction: TransactionId,
        current_timestamp: Option<u64>,
    ) -> Result<bool, NoteRecordError> {
        let new_state = self.state.consumed_locally(
            consumer_account,
            consumer_transaction,
            current_timestamp,
        )?;
        if let Some(new_state) = new_state {
            self.state = new_state;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Modifies the state of the note record to reflect that the transaction currently consuming
    /// the note was committed. Returns `true` if the state was changed.
    pub(crate) fn transaction_committed(
        &mut self,
        transaction_id: TransactionId,
        block_height: BlockNumber,
    ) -> Result<bool, NoteRecordError> {
        let new_state = self.state.transaction_committed(transaction_id, block_height)?;
        if let Some(new_state) = new_state {
            self.state = new_state;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}

// SERIALIZATION
// ================================================================================================

impl Serializable for InputNoteRecord {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.details.write_into(target);
        self.attachments.write_into(target);
        self.created_at.write_into(target);
        self.state.write_into(target);
    }
}

impl Deserializable for InputNoteRecord {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let details = NoteDetails::read_from(source)?;
        let attachments = NoteAttachments::read_from(source)?;
        let created_at = Option::<u64>::read_from(source)?;
        let state = InputNoteState::read_from(source)?;

        Ok(InputNoteRecord { details, attachments, created_at, state })
    }
}

// CONVERSION
// ================================================================================================

impl From<Note> for InputNoteRecord {
    fn from(value: Note) -> Self {
        let metadata = *value.metadata();
        let attachments = value.attachments().clone();
        Self {
            details: value.into(),
            attachments,
            created_at: None,
            state: ExpectedNoteState {
                metadata: Some(metadata),
                after_block_num: BlockNumber::from(0),
                tag: Some(metadata.tag()),
            }
            .into(),
        }
    }
}

impl From<InputNote> for InputNoteRecord {
    fn from(value: InputNote) -> Self {
        match value {
            InputNote::Authenticated { note, proof } => Self {
                attachments: note.attachments().clone(),
                details: note.clone().into(),
                created_at: None,
                state: UnverifiedNoteState {
                    metadata: *note.metadata(),
                    inclusion_proof: proof,
                }
                .into(),
            },
            InputNote::Unauthenticated { note } => note.into(),
        }
    }
}

impl TryInto<InputNote> for InputNoteRecord {
    type Error = NoteRecordError;

    fn try_into(self) -> Result<InputNote, Self::Error> {
        match (self.metadata(), self.inclusion_proof()) {
            (Some(metadata), Some(inclusion_proof)) => Ok(InputNote::authenticated(
                Note::with_attachments(
                    self.details.assets().clone(),
                    *metadata.partial_metadata(),
                    self.details.recipient().clone(),
                    self.attachments.clone(),
                ),
                inclusion_proof.clone(),
            )),
            (Some(metadata), None) => Ok(InputNote::unauthenticated(Note::with_attachments(
                self.details.assets().clone(),
                *metadata.partial_metadata(),
                self.details.recipient().clone(),
                self.attachments.clone(),
            ))),
            _ => Err(NoteRecordError::ConversionError(
                "Input Note Record does not contain metadata".to_string(),
            )),
        }
    }
}

impl TryInto<Note> for InputNoteRecord {
    type Error = NoteRecordError;

    fn try_into(self) -> Result<Note, Self::Error> {
        match self.metadata() {
            Some(metadata) => Ok(Note::with_attachments(
                self.details.assets().clone(),
                *metadata.partial_metadata(),
                self.details.recipient().clone(),
                self.attachments.clone(),
            )),
            None => Err(NoteRecordError::ConversionError(
                "Input Note Record does not contain metadata".to_string(),
            )),
        }
    }
}

impl TryInto<Note> for &InputNoteRecord {
    type Error = NoteRecordError;

    fn try_into(self) -> Result<Note, Self::Error> {
        match self.metadata() {
            Some(metadata) => Ok(Note::with_attachments(
                self.details.assets().clone(),
                *metadata.partial_metadata(),
                self.details.recipient().clone(),
                self.attachments.clone(),
            )),
            None => Err(NoteRecordError::ConversionError(
                "Input Note Record does not contain metadata".to_string(),
            )),
        }
    }
}

impl From<InputNoteRecord> for NoteDetails {
    fn from(value: InputNoteRecord) -> Self {
        value.details
    }
}

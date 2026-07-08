use alloc::string::ToString;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata, NoteTag};
use miden_protocol::transaction::TransactionId;

use super::{
    ConsumedExternalNoteState,
    InputNoteState,
    NoteStateHandler,
    NoteSubmissionData,
    ProcessingUnauthenticatedNoteState,
    UnverifiedNoteState,
};
use crate::store::NoteRecordError;

/// Information related to notes in the [`InputNoteState::Expected`] state.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpectedNoteState {
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information. The note metadata is only known if the note was created by the client or by
    /// retrieving it from the node. Imported or future notes may not have metadata.
    pub metadata: Option<NoteMetadata>,
    /// Block height after which the note is expected to be committed.
    pub after_block_num: BlockNumber,
    /// A tag used to identify the note. The tag may not be known if the note was imported without
    /// it or if it's a future note.
    pub tag: Option<NoteTag>,
}

impl NoteStateHandler for ExpectedNoteState {
    fn inclusion_proof_received(
        &self,
        inclusion_proof: NoteInclusionProof,
        metadata: NoteMetadata,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Ok(Some(UnverifiedNoteState { metadata, inclusion_proof }.into()))
    }

    fn consumed_externally(
        &self,
        nullifier_block_height: BlockNumber,
        consumer_account: Option<AccountId>,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Ok(Some(
            ConsumedExternalNoteState {
                nullifier_block_height,
                consumer_account,
                consumed_tx_order: None,
                metadata: self.metadata,
            }
            .into(),
        ))
    }

    fn block_header_received(
        &self,
        _note_id: NoteId,
        _block_header: &BlockHeader,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Err(NoteRecordError::StateTransitionError(
            "Can't verify an expected note".to_string(),
        ))
    }

    fn consumed_locally(
        &self,
        consumer_account: AccountId,
        consumer_transaction: TransactionId,
        current_timestamp: Option<u64>,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        match &self.metadata {
            None => Err(NoteRecordError::NoteNotConsumable(
                "Can't consume note without metadata".to_string(),
            )),
            Some(metadata) => {
                let submission_data = NoteSubmissionData {
                    submitted_at: current_timestamp,
                    consumer_account,
                    consumer_transaction,
                };

                Ok(Some(
                    ProcessingUnauthenticatedNoteState {
                        metadata: *metadata,
                        after_block_num: self.after_block_num,
                        submission_data,
                    }
                    .into(),
                ))
            },
        }
    }

    fn transaction_committed(
        &self,
        _transaction_id: TransactionId,
        _block_height: BlockNumber,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Err(NoteRecordError::InvalidStateTransition(
            "Only processing notes can be committed in a local transaction".to_string(),
        ))
    }

    fn metadata(&self) -> Option<&NoteMetadata> {
        self.metadata.as_ref()
    }

    fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        None
    }

    fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        None
    }
}

impl miden_tx::utils::serde::Serializable for ExpectedNoteState {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.metadata.write_into(target);
        self.after_block_num.write_into(target);
        self.tag.write_into(target);
    }
}

impl miden_tx::utils::serde::Deserializable for ExpectedNoteState {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let metadata = Option::<NoteMetadata>::read_from(source)?;
        let after_block_num = BlockNumber::read_from(source)?;
        let tag = Option::<NoteTag>::read_from(source)?;
        Ok(ExpectedNoteState { metadata, after_block_num, tag })
    }
}

impl From<ExpectedNoteState> for InputNoteState {
    fn from(state: ExpectedNoteState) -> Self {
        InputNoteState::Expected(state)
    }
}

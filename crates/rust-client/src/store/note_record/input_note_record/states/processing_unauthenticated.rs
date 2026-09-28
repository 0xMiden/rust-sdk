use alloc::string::ToString;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata};
use miden_protocol::transaction::TransactionId;

use super::{
    ConsumedExternalNoteState,
    ConsumedUnauthenticatedLocalNoteState,
    InputNoteState,
    NoteStateHandler,
    NoteSubmissionData,
};
use crate::store::NoteRecordError;

/// Information related to notes in the [`InputNoteState::ProcessingUnauthenticated`] state.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessingUnauthenticatedNoteState {
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information.
    pub metadata: NoteMetadata,
    /// Block height after which the note is expected to be committed.
    pub after_block_num: BlockNumber,
    /// Information about the submission of the note.
    pub submission_data: NoteSubmissionData,
}

impl NoteStateHandler for ProcessingUnauthenticatedNoteState {
    fn inclusion_proof_received(
        &self,
        _inclusion_proof: NoteInclusionProof,
        _metadata: NoteMetadata,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Ok(None)
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
                metadata: Some(self.metadata),
            }
            .into(),
        ))
    }

    fn block_header_received(
        &self,
        _note_id: NoteId,
        _block_header: &BlockHeader,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Ok(None)
    }

    fn consumed_locally(
        &self,
        _consumer_account: miden_protocol::account::AccountId,
        _consumer_transaction: miden_protocol::transaction::TransactionId,
        _current_timestamp: Option<u64>,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Err(NoteRecordError::NoteNotConsumable("Note being consumed".to_string()))
    }

    fn transaction_committed(
        &self,
        transaction_id: TransactionId,
        block_height: BlockNumber,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        if transaction_id != self.submission_data.consumer_transaction {
            return Err(NoteRecordError::StateTransitionError(
                "Transaction ID does not match the expected value".to_string(),
            ));
        }

        Ok(Some(
            ConsumedUnauthenticatedLocalNoteState {
                metadata: self.metadata,
                nullifier_block_height: block_height,
                submission_data: self.submission_data,
                consumed_tx_order: None,
            }
            .into(),
        ))
    }

    fn metadata(&self) -> Option<&NoteMetadata> {
        Some(&self.metadata)
    }

    fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        None
    }

    fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        Some(&self.submission_data.consumer_transaction)
    }
}

impl miden_tx::utils::serde::Serializable for ProcessingUnauthenticatedNoteState {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.metadata.write_into(target);
        self.after_block_num.write_into(target);
        self.submission_data.write_into(target);
    }
}

impl miden_tx::utils::serde::Deserializable for ProcessingUnauthenticatedNoteState {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let metadata = NoteMetadata::read_from(source)?;
        let after_block_num = BlockNumber::read_from(source)?;
        let submission_data = NoteSubmissionData::read_from(source)?;
        Ok(ProcessingUnauthenticatedNoteState {
            metadata,
            after_block_num,
            submission_data,
        })
    }
}

impl From<ProcessingUnauthenticatedNoteState> for InputNoteState {
    fn from(state: ProcessingUnauthenticatedNoteState) -> Self {
        InputNoteState::ProcessingUnauthenticated(state)
    }
}

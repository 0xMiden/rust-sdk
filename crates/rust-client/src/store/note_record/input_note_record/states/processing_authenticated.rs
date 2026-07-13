use alloc::string::ToString;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata};
use miden_protocol::transaction::TransactionId;

use super::{
    ConsumedAuthenticatedLocalNoteState,
    ConsumedExternalNoteState,
    InputNoteState,
    NoteStateHandler,
    NoteSubmissionData,
};
use crate::store::NoteRecordError;

/// Information related to notes in the [`InputNoteState::ProcessingAuthenticated`] state.
#[derive(Clone, Debug, PartialEq)]
pub struct ProcessingAuthenticatedNoteState {
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information.
    pub metadata: NoteMetadata,
    /// Inclusion proof for the note inside the chain block.
    pub inclusion_proof: NoteInclusionProof,
    /// Root of the note tree inside the block that verifies the note inclusion proof.
    pub block_note_root: Word,
    /// Information about the submission of the note.
    pub submission_data: NoteSubmissionData,
}

impl NoteStateHandler for ProcessingAuthenticatedNoteState {
    fn inclusion_proof_received(
        &self,
        inclusion_proof: NoteInclusionProof,
        metadata: NoteMetadata,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        if self.inclusion_proof != inclusion_proof || self.metadata != metadata {
            return Err(NoteRecordError::StateTransitionError(
                "Inclusion proof or metadata do not match the expected values".to_string(),
            ));
        }
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
        _consumer_account: AccountId,
        _consumer_transaction: TransactionId,
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
            ConsumedAuthenticatedLocalNoteState {
                metadata: self.metadata,
                inclusion_proof: self.inclusion_proof.clone(),
                block_note_root: self.block_note_root,
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
        Some(&self.inclusion_proof)
    }

    fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        Some(&self.submission_data.consumer_transaction)
    }
}

impl miden_tx::utils::serde::Serializable for ProcessingAuthenticatedNoteState {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.metadata.write_into(target);
        self.inclusion_proof.write_into(target);
        self.block_note_root.write_into(target);
        self.submission_data.write_into(target);
    }
}

impl miden_tx::utils::serde::Deserializable for ProcessingAuthenticatedNoteState {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let metadata = NoteMetadata::read_from(source)?;
        let inclusion_proof = NoteInclusionProof::read_from(source)?;
        let block_note_root = Word::read_from(source)?;
        let submission_data = NoteSubmissionData::read_from(source)?;
        Ok(ProcessingAuthenticatedNoteState {
            metadata,
            inclusion_proof,
            block_note_root,
            submission_data,
        })
    }
}

impl From<ProcessingAuthenticatedNoteState> for InputNoteState {
    fn from(state: ProcessingAuthenticatedNoteState) -> Self {
        InputNoteState::ProcessingAuthenticated(state)
    }
}

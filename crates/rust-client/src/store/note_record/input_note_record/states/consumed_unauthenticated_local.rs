use alloc::string::ToString;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata};
use miden_protocol::transaction::TransactionId;

use super::{InputNoteState, NoteStateHandler, NoteSubmissionData};
use crate::store::NoteRecordError;

/// Information related to notes in the [`InputNoteState::ConsumedUnauthenticatedLocal`] state.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsumedUnauthenticatedLocalNoteState {
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information.
    pub metadata: NoteMetadata,
    /// Block height at which the note was nullified.
    pub nullifier_block_height: BlockNumber,
    /// Information about the submission of the note.
    pub submission_data: NoteSubmissionData,
    /// Per-account position of the consuming transaction within the account's execution chain for
    /// the block. `None` if the order has not been determined yet.
    pub consumed_tx_order: Option<u32>,
}

impl NoteStateHandler for ConsumedUnauthenticatedLocalNoteState {
    fn inclusion_proof_received(
        &self,
        _inclusion_proof: NoteInclusionProof,
        _metadata: NoteMetadata,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Ok(None)
    }

    fn consumed_externally(
        &self,
        _nullifier_block_height: BlockNumber,
        _consumer_account: Option<AccountId>,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        Ok(None)
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
        Err(NoteRecordError::NoteNotConsumable("Note already consumed".to_string()))
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
        Some(&self.metadata)
    }

    fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        None
    }

    fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        Some(&self.submission_data.consumer_transaction)
    }
}

impl miden_tx::utils::serde::Serializable for ConsumedUnauthenticatedLocalNoteState {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.metadata.write_into(target);
        self.nullifier_block_height.write_into(target);
        self.submission_data.write_into(target);
        self.consumed_tx_order.write_into(target);
    }
}

impl miden_tx::utils::serde::Deserializable for ConsumedUnauthenticatedLocalNoteState {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let metadata = NoteMetadata::read_from(source)?;
        let nullifier_block_height = BlockNumber::read_from(source)?;
        let submission_data = NoteSubmissionData::read_from(source)?;
        let consumed_tx_order = Option::<u32>::read_from(source)?;
        Ok(ConsumedUnauthenticatedLocalNoteState {
            metadata,
            nullifier_block_height,
            submission_data,
            consumed_tx_order,
        })
    }
}

impl From<ConsumedUnauthenticatedLocalNoteState> for InputNoteState {
    fn from(state: ConsumedUnauthenticatedLocalNoteState) -> Self {
        InputNoteState::ConsumedUnauthenticatedLocal(state)
    }
}

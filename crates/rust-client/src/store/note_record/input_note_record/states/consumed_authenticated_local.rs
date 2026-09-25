use alloc::string::ToString;

use miden_objects::DecodeMessageExt;
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata};
use miden_protocol::transaction::TransactionId;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use super::{InputNoteState, NoteStateHandler, NoteSubmissionData};
use crate::store::NoteRecordError;
use crate::store::proto::{self, ProtoDecodeError};

/// Information related to notes in the [`InputNoteState::ConsumedAuthenticatedLocal`] state.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsumedAuthenticatedLocalNoteState {
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information.
    pub metadata: NoteMetadata,
    /// Inclusion proof for the note inside the chain block.
    pub inclusion_proof: NoteInclusionProof,
    /// Root of the note tree inside the block that verifies the note inclusion proof.
    pub block_note_root: Word,
    /// Block height at which the note was nullified.
    pub nullifier_block_height: BlockNumber,
    /// Information about the submission of the note.
    pub submission_data: NoteSubmissionData,
    /// Per-account position of the consuming transaction within the account's execution chain for
    /// the block. `None` if the order has not been determined yet.
    pub consumed_tx_order: Option<u32>,
}

impl NoteStateHandler for ConsumedAuthenticatedLocalNoteState {
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
        Some(&self.inclusion_proof)
    }

    fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        Some(&self.submission_data.consumer_transaction)
    }
}

impl Serializable for ConsumedAuthenticatedLocalNoteState {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.metadata.write_into(target);
        self.inclusion_proof.write_into(target);
        self.block_note_root.write_into(target);
        self.nullifier_block_height.write_into(target);
        self.submission_data.write_into(target);
        self.consumed_tx_order.write_into(target);
    }
}

impl Deserializable for ConsumedAuthenticatedLocalNoteState {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let metadata = NoteMetadata::read_from(source)?;
        let inclusion_proof = NoteInclusionProof::read_from(source)?;
        let block_note_root = Word::read_from(source)?;
        let nullifier_block_height = BlockNumber::read_from(source)?;
        let submission_data = NoteSubmissionData::read_from(source)?;
        let consumed_tx_order = Option::<u32>::read_from(source)?;
        Ok(ConsumedAuthenticatedLocalNoteState {
            metadata,
            inclusion_proof,
            block_note_root,
            nullifier_block_height,
            submission_data,
            consumed_tx_order,
        })
    }
}

impl From<&ConsumedAuthenticatedLocalNoteState>
    for proto::input_note_state::ConsumedAuthenticatedLocal
{
    fn from(state: &ConsumedAuthenticatedLocalNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            inclusion_proof: Some((&state.inclusion_proof).into()),
            block_note_root: Some(state.block_note_root.into()),
            nullifier_block_height: Some(state.nullifier_block_height.into()),
            submission_data: Some((&state.submission_data).into()),
            consumed_tx_order: state.consumed_tx_order,
        }
    }
}

impl TryFrom<proto::input_note_state::ConsumedAuthenticatedLocal>
    for ConsumedAuthenticatedLocalNoteState
{
    type Error = ProtoDecodeError;

    fn try_from(
        state: proto::input_note_state::ConsumedAuthenticatedLocal,
    ) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "consumed authenticated local note state";

        Ok(ConsumedAuthenticatedLocalNoteState {
            metadata: proto::required(state.metadata, MESSAGE, "metadata")?.decode_and_verify()?,
            inclusion_proof: proto::required(state.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
            block_note_root: proto::required(state.block_note_root, MESSAGE, "block note root")?
                .try_into()?,
            nullifier_block_height: proto::required(
                state.nullifier_block_height,
                MESSAGE,
                "nullifier block height",
            )?
            .decode_and_verify()?,
            submission_data: proto::required(state.submission_data, MESSAGE, "submission data")?
                .try_into()?,
            consumed_tx_order: state.consumed_tx_order,
        })
    }
}

impl From<ConsumedAuthenticatedLocalNoteState> for InputNoteState {
    fn from(state: ConsumedAuthenticatedLocalNoteState) -> Self {
        InputNoteState::ConsumedAuthenticatedLocal(state)
    }
}

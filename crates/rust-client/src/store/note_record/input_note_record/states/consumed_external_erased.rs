use alloc::string::ToString;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata, Nullifier};
use miden_protocol::transaction::TransactionId;

use super::{InputNoteState, NoteStateHandler};
use crate::store::NoteRecordError;

/// Information related to notes in the [`InputNoteState::ConsumedExternalErased`] state.
///
/// A record enters this state when a tracked account consumes a note as an unauthenticated input
/// (typically an erased note, created and consumed in the same batch) whose full details the client
/// never held, only the note header carried in the consuming transaction. With no authoritative
/// [`miden_protocol::note::NoteDetails`], the note id cannot be derived the usual way (from the
/// details commitment and metadata), so it is stored in the state directly.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsumedExternalErasedNoteState {
    /// The note id, stored directly. Unlike full records, it cannot be derived from the record's
    /// details, which are a placeholder for this state.
    pub note_id: NoteId,
    /// The note nullifier, stored directly for the same reason as `note_id`. It cannot be
    /// recomputed from the placeholder details, so it is taken from the consuming
    /// transaction's input commitment.
    pub nullifier: Nullifier,
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information.
    pub metadata: NoteMetadata,
    /// Block height at which the note was nullified.
    pub nullifier_block_height: BlockNumber,
    /// The account that consumed the note, if it is tracked by this client.
    pub consumer_account: Option<AccountId>,
    /// Per-account position of the consuming transaction within the account's execution chain
    /// for the block. `None` if the order has not been determined yet.
    pub consumed_tx_order: Option<u32>,
}

impl NoteStateHandler for ConsumedExternalErasedNoteState {
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
        _consumer_account: AccountId,
        _consumer_transaction: TransactionId,
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
        None
    }
}

impl miden_tx::utils::serde::Serializable for ConsumedExternalErasedNoteState {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.note_id.write_into(target);
        self.nullifier.write_into(target);
        self.metadata.write_into(target);
        self.nullifier_block_height.write_into(target);
        self.consumer_account.write_into(target);
        self.consumed_tx_order.write_into(target);
    }
}

impl miden_tx::utils::serde::Deserializable for ConsumedExternalErasedNoteState {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let note_id = NoteId::read_from(source)?;
        let nullifier = Nullifier::read_from(source)?;
        let metadata = NoteMetadata::read_from(source)?;
        let nullifier_block_height = BlockNumber::read_from(source)?;
        let consumer_account = Option::<AccountId>::read_from(source)?;
        let consumed_tx_order = Option::<u32>::read_from(source)?;
        Ok(ConsumedExternalErasedNoteState {
            note_id,
            nullifier,
            metadata,
            nullifier_block_height,
            consumer_account,
            consumed_tx_order,
        })
    }
}

impl From<ConsumedExternalErasedNoteState> for InputNoteState {
    fn from(state: ConsumedExternalErasedNoteState) -> Self {
        InputNoteState::ConsumedExternalErased(state)
    }
}

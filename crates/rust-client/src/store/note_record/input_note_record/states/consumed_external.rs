use alloc::string::ToString;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata};
use miden_protocol::transaction::TransactionId;
use miden_tx::utils::serde::Deserializable;

use super::{InputNoteState, NoteStateHandler};
use crate::store::NoteRecordError;

/// Information related to notes in the [`InputNoteState::ConsumedExternal`] state.
///
/// A note enters this state when its nullifier appears on-chain but the consuming transaction was
/// not submitted by this client.
#[derive(Clone, Debug, PartialEq)]
pub struct ConsumedExternalNoteState {
    /// Block height at which the note was nullified.
    pub nullifier_block_height: BlockNumber,
    /// The account that consumed the note, if it is tracked by this client.
    pub consumer_account: Option<AccountId>,
    /// Per-account position of the consuming transaction within the account's execution chain
    /// for the block. `None` if the order has not been determined yet.
    pub consumed_tx_order: Option<u32>,
    /// Metadata associated with the note (sender, note type, tag and other additional
    /// information), retained through consumption so the note ID stays recoverable. `None` when
    /// the prior state had no metadata (e.g. a note imported from bare
    /// `NoteFile::NoteDetails`, or a record read back from the legacy on-disk layout that
    /// predates this field).
    pub metadata: Option<NoteMetadata>,
}

impl NoteStateHandler for ConsumedExternalNoteState {
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
        self.metadata.as_ref()
    }

    fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        None
    }

    fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        None
    }
}

impl ConsumedExternalNoteState {
    /// Deserializes the legacy layout stored under [`InputNoteState::STATE_CONSUMED_EXTERNAL`],
    /// which predates the `metadata` field. Records written in that format carry no metadata,
    /// so `metadata` is set to `None`.
    pub(crate) fn read_from_legacy<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let nullifier_block_height = BlockNumber::read_from(source)?;
        let consumer_account = Option::<AccountId>::read_from(source)?;
        let consumed_tx_order = Option::<u32>::read_from(source)?;
        Ok(ConsumedExternalNoteState {
            nullifier_block_height,
            consumer_account,
            consumed_tx_order,
            metadata: None,
        })
    }
}

impl miden_tx::utils::serde::Serializable for ConsumedExternalNoteState {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.nullifier_block_height.write_into(target);
        self.consumer_account.write_into(target);
        self.consumed_tx_order.write_into(target);
        self.metadata.write_into(target);
    }
}

impl miden_tx::utils::serde::Deserializable for ConsumedExternalNoteState {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let nullifier_block_height = BlockNumber::read_from(source)?;
        let consumer_account = Option::<AccountId>::read_from(source)?;
        let consumed_tx_order = Option::<u32>::read_from(source)?;
        let metadata = Option::<NoteMetadata>::read_from(source)?;
        Ok(ConsumedExternalNoteState {
            nullifier_block_height,
            consumer_account,
            consumed_tx_order,
            metadata,
        })
    }
}

impl From<ConsumedExternalNoteState> for InputNoteState {
    fn from(state: ConsumedExternalNoteState) -> Self {
        InputNoteState::ConsumedExternal(state)
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec::Vec;

    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_tx::utils::serde::{Deserializable, Serializable};

    use crate::store::InputNoteState;

    /// A row written by an older client uses discriminant 8 with three fields and no metadata.
    /// Reading it back must succeed and leave `metadata` empty, so existing stores keep working
    /// after upgrading to a client that writes the metadata-bearing layout.
    #[test]
    fn legacy_consumed_external_blob_decodes_without_metadata() {
        let mut legacy_bytes = Vec::new();
        legacy_bytes.push(InputNoteState::STATE_CONSUMED_EXTERNAL);
        BlockNumber::from(7u32).write_into(&mut legacy_bytes);
        None::<AccountId>.write_into(&mut legacy_bytes);
        None::<u32>.write_into(&mut legacy_bytes);

        let state = InputNoteState::read_from_bytes(&legacy_bytes).unwrap();
        let InputNoteState::ConsumedExternal(inner) = state else {
            panic!("expected ConsumedExternal");
        };
        assert_eq!(inner.nullifier_block_height, BlockNumber::from(7u32));
        assert_eq!(inner.metadata, None);
    }
}

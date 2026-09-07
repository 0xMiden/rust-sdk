use alloc::string::ToString;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata};
use miden_protocol::transaction::TransactionId;

use super::{
    CommittedNoteState,
    ConsumedExternalNoteState,
    InputNoteState,
    InvalidNoteState,
    NoteStateHandler,
    NoteSubmissionData,
    ProcessingUnauthenticatedNoteState,
};
use crate::store::NoteRecordError;

/// Information related to notes in the [`InputNoteState::Unverified`] state.
#[derive(Clone, Debug, PartialEq)]
pub struct UnverifiedNoteState {
    /// Metadata associated with the note, including sender, note type, tag and other additional
    /// information.
    pub metadata: NoteMetadata,
    /// Inclusion proof for the note inside the chain block. This proof isn't yet verified.
    pub inclusion_proof: NoteInclusionProof,
}

impl NoteStateHandler for UnverifiedNoteState {
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
                metadata: Some(self.metadata),
            }
            .into(),
        ))
    }

    fn block_header_received(
        &self,
        note_id: NoteId,
        block_header: &BlockHeader,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        // The proof authenticates the note against the note root of the block it names, so a
        // header for any other block cannot confirm it, however well the path verifies: with an
        // honest node this should never trigger.
        let proof_authenticates_note = self.inclusion_proof.location().block_num()
            == block_header.block_num()
            && self
                .inclusion_proof
                .note_path()
                .verify(
                    self.inclusion_proof.location().block_note_tree_index().into(),
                    note_id.as_word(),
                    &block_header.note_root(),
                )
                .is_ok();

        if proof_authenticates_note {
            Ok(Some(
                CommittedNoteState {
                    inclusion_proof: self.inclusion_proof.clone(),
                    metadata: self.metadata,
                    block_note_root: block_header.note_root(),
                }
                .into(),
            ))
        } else {
            Ok(Some(
                InvalidNoteState {
                    metadata: self.metadata,
                    invalid_inclusion_proof: self.inclusion_proof.clone(),
                    block_note_root: block_header.note_root(),
                }
                .into(),
            ))
        }
    }

    fn consumed_locally(
        &self,
        consumer_account: miden_protocol::account::AccountId,
        consumer_transaction: miden_protocol::transaction::TransactionId,
        _current_timestamp: Option<u64>,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        let submission_data = NoteSubmissionData {
            submitted_at: None,
            consumer_account,
            consumer_transaction,
        };

        let after_block_num =
            self.inclusion_proof.location().block_num().as_u32().saturating_sub(1);
        Ok(Some(
            ProcessingUnauthenticatedNoteState {
                metadata: self.metadata,
                after_block_num: BlockNumber::from(after_block_num),
                submission_data,
            }
            .into(),
        ))
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
        None
    }
}

impl miden_tx::utils::serde::Serializable for UnverifiedNoteState {
    fn write_into<W: miden_tx::utils::serde::ByteWriter>(&self, target: &mut W) {
        self.metadata.write_into(target);
        self.inclusion_proof.write_into(target);
    }
}

impl miden_tx::utils::serde::Deserializable for UnverifiedNoteState {
    fn read_from<R: miden_tx::utils::serde::ByteReader>(
        source: &mut R,
    ) -> Result<Self, miden_tx::utils::serde::DeserializationError> {
        let metadata = NoteMetadata::read_from(source)?;
        let inclusion_proof = NoteInclusionProof::read_from(source)?;
        Ok(UnverifiedNoteState { metadata, inclusion_proof })
    }
}

impl From<UnverifiedNoteState> for InputNoteState {
    fn from(state: UnverifiedNoteState) -> Self {
        InputNoteState::Unverified(state)
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::Word;
    use miden_protocol::account::{AccountIdVersion, AccountType, AssetCallbackFlag};
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::{NoteAttachments, NoteTag, NoteType, PartialNoteMetadata};
    use miden_protocol::transaction::TransactionKernel;

    use super::*;

    /// An unverified note whose empty path authenticates it against a note root equal to the
    /// note's own ID, in the block the proof names.
    fn unverified_note(proof_block: u32) -> (NoteId, UnverifiedNoteState) {
        let note_id = NoteId::from_raw(Word::from([1u32, 2, 3, 4]));
        let sender = AccountId::dummy(
            [1; 15],
            AccountIdVersion::Version1,
            AccountType::Public,
            AssetCallbackFlag::Disabled,
        );
        let metadata = NoteMetadata::new(
            PartialNoteMetadata::new(sender, NoteType::Private).with_tag(NoteTag::new(7)),
            &NoteAttachments::empty(),
        );
        let inclusion_proof = NoteInclusionProof::new(
            proof_block.into(),
            0,
            SparseMerklePath::from_parts(0, alloc::vec::Vec::new()).unwrap(),
        )
        .unwrap();

        (note_id, UnverifiedNoteState { metadata, inclusion_proof })
    }

    /// A header for `block_num` whose note root is `note_root`, so the same root can be placed in
    /// more than one block.
    fn header(block_num: u32, note_root: Word) -> BlockHeader {
        BlockHeader::mock(block_num, None, Some(note_root), &[], TransactionKernel.to_commitment())
    }

    #[test]
    fn header_for_the_named_block_commits_the_note() {
        let (note_id, state) = unverified_note(4);

        let committed = state
            .block_header_received(note_id, &header(4, note_id.as_word()))
            .unwrap()
            .unwrap();

        assert!(matches!(committed, InputNoteState::Committed(_)), "got {committed:?}");
    }

    #[test]
    fn header_for_another_block_invalidates_the_note_despite_a_matching_note_root() {
        let (note_id, state) = unverified_note(4);

        // The path still verifies against this header, since its note root is the same. Only the
        // block the proof names sets the two apart.
        let invalid = state
            .block_header_received(note_id, &header(9, note_id.as_word()))
            .unwrap()
            .unwrap();

        assert!(matches!(invalid, InputNoteState::Invalid(_)), "got {invalid:?}");
    }
}

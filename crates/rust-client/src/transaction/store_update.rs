use alloc::vec::Vec;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteDetails, NoteTag};
use miden_protocol::transaction::ExecutedTransaction;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use crate::note::NoteUpdateTracker;
use crate::sync::NoteTagRecord;

// TRANSACTION STORE UPDATE
// ================================================================================================

/// Represents the changes that need to be applied to the client store as a result of a
/// transaction execution.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionStoreUpdate {
    /// Details of the executed transaction to be inserted.
    executed_transaction: ExecutedTransaction,
    /// Block number at which the transaction was submitted.
    submission_height: BlockNumber,
    /// Future notes that are expected to be created as a result of the transaction.
    future_notes: Vec<(NoteDetails, NoteTag)>,
    /// Information about note changes after the transaction execution.
    note_updates: NoteUpdateTracker,
    /// New note tags to be tracked.
    new_tags: Vec<NoteTagRecord>,
}

impl TransactionStoreUpdate {
    /// Creates a new [`TransactionStoreUpdate`] instance populated with all relevant note data.
    ///
    /// # Arguments
    /// - `executed_transaction`: The executed transaction details.
    /// - `submission_height`: The block number at which the transaction was submitted.
    /// - `note_updates`: The note updates that need to be applied to the store after the
    ///   transaction execution.
    /// - `future_notes`: Notes expected to be received in follow-up transactions (e.g. swap
    ///   paybacks).
    /// - `new_tags`: New note tags that need to be tracked because of created notes.
    pub fn new(
        executed_transaction: ExecutedTransaction,
        submission_height: BlockNumber,
        note_updates: NoteUpdateTracker,
        future_notes: Vec<(NoteDetails, NoteTag)>,
        new_tags: Vec<NoteTagRecord>,
    ) -> Self {
        Self {
            executed_transaction,
            submission_height,
            future_notes,
            note_updates,
            new_tags,
        }
    }
    /// Returns the executed transaction.
    pub fn executed_transaction(&self) -> &ExecutedTransaction {
        &self.executed_transaction
    }

    /// Returns the block number at which the transaction was submitted.
    pub fn submission_height(&self) -> BlockNumber {
        self.submission_height
    }

    /// Returns the future notes that should be tracked as a result of the transaction.
    pub fn future_notes(&self) -> &[(NoteDetails, NoteTag)] {
        &self.future_notes
    }

    /// Returns the note updates that need to be applied after the transaction execution.
    pub fn note_updates(&self) -> &NoteUpdateTracker {
        &self.note_updates
    }

    /// Returns the new tags that were created as part of the transaction.
    pub fn new_tags(&self) -> &[NoteTagRecord] {
        &self.new_tags
    }
}

impl Serializable for TransactionStoreUpdate {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.executed_transaction.write_into(target);
        self.submission_height.write_into(target);
        self.future_notes.write_into(target);
        self.note_updates.write_into(target);
        self.new_tags.write_into(target);
    }
}

impl Deserializable for TransactionStoreUpdate {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let executed_transaction = ExecutedTransaction::read_from(source)?;
        let submission_height = BlockNumber::read_from(source)?;
        let future_notes = Vec::<(NoteDetails, NoteTag)>::read_from(source)?;
        let note_updates = NoteUpdateTracker::read_from(source)?;
        let new_tags = Vec::<NoteTagRecord>::read_from(source)?;

        Ok(Self {
            executed_transaction,
            submission_height,
            future_notes,
            note_updates,
            new_tags,
        })
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use miden_protocol::asset::{Asset, FungibleAsset};
    use miden_protocol::note::NoteType;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_SENDER,
    };
    use miden_testing::{MockChainBuilder, MockTransactionInput};

    use super::*;
    use crate::note::NoteUpdateTracker;
    use crate::store::InputNoteRecord;
    use crate::sync::NoteTagRecord;

    #[tokio::test]
    async fn transaction_store_update_serialization_roundtrip() {
        // Build a minimal MockChain with an account consuming a P2ID note so that we can
        // produce a real `ExecutedTransaction`.
        let sender_id = ACCOUNT_ID_SENDER.try_into().unwrap();
        let faucet_id = ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET.try_into().unwrap();
        let asset = Asset::Fungible(FungibleAsset::new(faucet_id, 100u64).unwrap());

        let mut builder = MockChainBuilder::new();
        let account = builder.add_existing_mock_account(miden_testing::Auth::IncrNonce).unwrap();
        let note = builder
            .add_p2id_note(sender_id, account.id(), &[asset], NoteType::Public)
            .unwrap();
        let mut chain = builder.build().unwrap();
        chain.prove_next_block().unwrap();

        let executed_tx = Box::pin(
            chain
                .build_transaction(MockTransactionInput::Account(account.clone()))
                .unauthenticated_input_note(note.clone())
                .build()
                .unwrap()
                .execute(),
        )
        .await
        .unwrap();

        // Build non-trivial `note_updates` and `new_tags` so that the round-trip covers all
        // fields that were previously dropped.
        let input_note = InputNoteRecord::from(note.clone());
        let note_updates = NoteUpdateTracker::for_transaction_updates([input_note], [], []);

        let tag = NoteTag::with_account_target(account.id());
        let new_tags = vec![NoteTagRecord::with_account_source(tag, account.id())];

        let future_notes = vec![(Into::<NoteDetails>::into(note.clone()), tag)];

        let update = TransactionStoreUpdate::new(
            executed_tx,
            BlockNumber::from(42u32),
            note_updates,
            future_notes,
            new_tags,
        );

        let bytes = update.to_bytes();
        let deserialized = TransactionStoreUpdate::read_from_bytes(&bytes).unwrap();

        assert_eq!(update, deserialized);
    }
}

use alloc::vec::Vec;

use miden_client_proto::{self as proto, ProtoDecodeError, ProtobufValue, read_upstream};
use miden_protocol::account::AccountPatch;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteDetails, NoteTag};
use miden_protocol::transaction::{
    ExecutedTransaction,
    InputNote,
    InputNotes,
    RawOutputNotes,
    TransactionArgs,
    TransactionId,
    TransactionInputs,
};
use miden_protocol::utils::serde::Serializable;

// TRANSACTION RESULT
// ================================================================================================

/// Represents the result of executing a transaction by the client.
///
/// It contains an [`ExecutedTransaction`], and a list of `future_notes` that we expect to receive
/// in the future (you can check at swap notes for an example of this).
#[derive(Clone, Debug, PartialEq)]
pub struct TransactionResult {
    transaction: ExecutedTransaction,
    future_notes: Vec<(NoteDetails, NoteTag)>,
}

impl TransactionResult {
    /// Wraps an executed transaction with the notes the client expects to receive later.
    pub fn new(
        transaction: ExecutedTransaction,
        future_notes: Vec<(NoteDetails, NoteTag)>,
    ) -> Self {
        Self { transaction, future_notes }
    }

    /// Returns a unique identifier of this transaction.
    pub fn id(&self) -> TransactionId {
        self.transaction.id()
    }

    /// Returns the [`ExecutedTransaction`].
    pub fn executed_transaction(&self) -> &ExecutedTransaction {
        &self.transaction
    }

    /// Returns the output notes that were generated as a result of the transaction execution.
    pub fn created_notes(&self) -> &RawOutputNotes {
        self.transaction.output_notes()
    }

    /// Returns the list of notes that might be created in the future as a result of the transaction
    /// execution.
    pub fn future_notes(&self) -> &[(NoteDetails, NoteTag)] {
        &self.future_notes
    }

    /// Returns the block against which the transaction was executed.
    pub fn block_num(&self) -> BlockNumber {
        self.transaction.block_header().block_num()
    }

    /// Returns transaction's [`TransactionArgs`].
    pub fn transaction_arguments(&self) -> &TransactionArgs {
        self.transaction.tx_args()
    }

    /// Returns a reference to the [`TransactionInputs`].
    pub fn tx_inputs(&self) -> &TransactionInputs {
        self.transaction.tx_inputs()
    }

    /// Returns the [`AccountPatch`] that describes the change of state for the executing account.
    pub fn account_patch(&self) -> &AccountPatch {
        self.transaction.account_patch()
    }

    /// Returns input notes that were consumed as part of the transaction.
    pub fn consumed_notes(&self) -> &InputNotes<InputNote> {
        self.transaction.tx_inputs().input_notes()
    }
}

impl From<&TransactionResult> for TransactionInputs {
    fn from(value: &TransactionResult) -> Self {
        value.executed_transaction().tx_inputs().clone()
    }
}

impl From<TransactionResult> for TransactionInputs {
    fn from(value: TransactionResult) -> Self {
        let (inputs, ..) = value.transaction.into_parts();
        inputs
    }
}

impl From<TransactionResult> for ExecutedTransaction {
    fn from(tx_result: TransactionResult) -> ExecutedTransaction {
        tx_result.transaction
    }
}

// SERIALIZATION
// ================================================================================================

impl ProtobufValue for TransactionResult {
    type Message = proto::TransactionResult;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            executed_transaction: self.transaction.to_bytes(),
            future_notes: self.future_notes.iter().map(ProtobufValue::to_proto).collect(),
        }
    }

    fn from_proto(result: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(TransactionResult::new(
            read_upstream(&result.executed_transaction)?,
            result
                .future_notes
                .into_iter()
                .map(<(NoteDetails, NoteTag)>::from_proto)
                .collect::<Result<Vec<_>, _>>()?,
        ))
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::boxed::Box;

    use miden_client_proto::{decode, encode};
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::{Asset, FungibleAsset};
    use miden_protocol::note::{NoteDetails, NoteTag, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_SENDER,
    };
    use miden_testing::{Auth, MockChainBuilder, MockTransactionInput};

    use super::TransactionResult;

    /// The executed transaction consumes a P2ID note, and the result expects the same note as a
    /// future note, so both fields carry data.
    #[tokio::test]
    async fn transaction_result_round_trips() {
        let sender_id = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let asset = Asset::from(FungibleAsset::new(faucet_id, 100).unwrap());

        let mut builder = MockChainBuilder::new();
        let account = builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
        let note = builder
            .add_p2id_note(sender_id, account.id(), &[asset], NoteType::Public)
            .unwrap();
        let mut chain = builder.build().unwrap();
        chain.prove_next_block().unwrap();

        let executed_transaction = Box::pin(
            chain
                .build_transaction(MockTransactionInput::Account(account.clone()))
                .unauthenticated_input_note(note.clone())
                .build()
                .unwrap()
                .execute(),
        )
        .await
        .unwrap();

        let future_notes =
            vec![(NoteDetails::from(note), NoteTag::with_account_target(account.id()))];
        let result = TransactionResult::new(executed_transaction, future_notes);

        assert_eq!(decode::<TransactionResult>(&encode(&result)).unwrap(), result);
    }
}

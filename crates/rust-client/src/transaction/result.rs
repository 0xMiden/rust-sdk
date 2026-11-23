use alloc::vec::Vec;

use miden_client_core::transaction::TransactionOutcome;
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
    TransactionWitness,
};

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

    /// Returns the [`TransactionWitness`] that a prover needs to prove the transaction.
    ///
    /// Pass it to [`Client::prove_transaction_witness`](crate::Client::prove_transaction_witness)
    /// with [`Self::id`].
    pub fn witness(&self) -> TransactionWitness {
        TransactionWitness::new_unchecked(
            self.transaction.tx_inputs().clone(),
            self.transaction.final_account().clone(),
            self.transaction.expiration_block_num(),
        )
    }

    /// Returns the [`TransactionOutcome`], the parts of the transaction that the store records.
    ///
    /// Pass it to [`Client::get_outcome_store_update`](crate::Client::get_outcome_store_update)
    /// with [`Self::future_notes`].
    pub fn outcome(&self) -> TransactionOutcome {
        TransactionOutcome::from(&self.transaction)
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

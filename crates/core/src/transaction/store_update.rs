use alloc::vec::Vec;

use miden_protocol::account::{AccountHeader, AccountId};
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteDetails, NoteTag};
use miden_protocol::transaction::{
    ExecutedTransaction,
    TransactionEffects,
    TransactionId,
    TransactionScript,
};

use crate::store::NoteUpdates;
use crate::sync::NoteTagRecord;

// TRANSACTION OUTCOME
// ================================================================================================

/// The parts of an executed transaction that the store records.
///
/// It does not hold the transaction inputs, so it is much smaller than an [`ExecutedTransaction`].
/// A transaction cannot be proven from it. To prove a transaction, use its
/// [`TransactionWitness`](miden_protocol::transaction::TransactionWitness).
///
/// The two account headers must belong to the account of the account patch. The final header must
/// commit to the final state commitment of the effects. The initial header must commit to the
/// initial state commitment of the effects, except for a new account: for a new account, the
/// effects hold [`Word::empty`](miden_protocol::Word::empty) as the initial state commitment, and
/// the initial header has nonce zero. A value built from an [`ExecutedTransaction`] always agrees.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionOutcome {
    /// The effects of the transaction.
    effects: TransactionEffects,
    /// The header of the account before the transaction.
    initial_account: AccountHeader,
    /// The header of the account after the transaction.
    final_account: AccountHeader,
    /// The script that the transaction executed, if any.
    tx_script: Option<TransactionScript>,
}

impl TransactionOutcome {
    /// Creates a new [`TransactionOutcome`].
    pub fn new(
        effects: TransactionEffects,
        initial_account: AccountHeader,
        final_account: AccountHeader,
        tx_script: Option<TransactionScript>,
    ) -> Self {
        Self {
            effects,
            initial_account,
            final_account,
            tx_script,
        }
    }

    /// Returns the ID of the transaction.
    pub fn id(&self) -> TransactionId {
        self.effects.transaction_id()
    }

    /// Returns the ID of the account that executed the transaction.
    pub fn account_id(&self) -> AccountId {
        self.final_account.id()
    }

    /// Returns the effects of the transaction.
    pub fn effects(&self) -> &TransactionEffects {
        &self.effects
    }

    /// Returns the header of the account before the transaction.
    ///
    /// For a new account, its commitment is the commitment of the new account, and not the initial
    /// state commitment of the effects.
    pub fn initial_account(&self) -> &AccountHeader {
        &self.initial_account
    }

    /// Returns the header of the account after the transaction.
    pub fn final_account(&self) -> &AccountHeader {
        &self.final_account
    }

    /// Returns the script that the transaction executed, if any.
    pub fn tx_script(&self) -> Option<&TransactionScript> {
        self.tx_script.as_ref()
    }
}

impl From<&ExecutedTransaction> for TransactionOutcome {
    fn from(transaction: &ExecutedTransaction) -> Self {
        Self::new(
            TransactionEffects::from(transaction),
            AccountHeader::from(transaction.initial_account()),
            transaction.final_account().clone(),
            transaction.tx_args().tx_script().cloned(),
        )
    }
}

// TRANSACTION STORE UPDATE
// ================================================================================================

/// Represents the changes that need to be applied to the client store as a result of a transaction
/// execution.
#[derive(Debug, Clone, PartialEq)]
pub struct TransactionStoreUpdate {
    /// The executed transaction to be inserted.
    transaction: TransactionOutcome,
    /// Block number at which the transaction was submitted.
    submission_height: BlockNumber,
    /// Future notes that are expected to be created as a result of the transaction.
    future_notes: Vec<(NoteDetails, NoteTag)>,
    /// Information about note changes after the transaction execution.
    note_updates: NoteUpdates,
    /// New note tags to be tracked.
    new_tags: Vec<NoteTagRecord>,
}

impl TransactionStoreUpdate {
    /// Creates a new [`TransactionStoreUpdate`] instance populated with all relevant note data.
    ///
    /// # Arguments
    /// - `transaction`: The parts of the executed transaction that the store records.
    /// - `submission_height`: The block number at which the transaction was submitted.
    /// - `note_updates`: The note updates that need to be applied to the store after the
    ///   transaction execution.
    /// - `future_notes`: Notes expected to be received in follow-up transactions (e.g. swap
    ///   paybacks).
    /// - `new_tags`: New note tags that need to be tracked because of created notes. A tag with a
    ///   [`NoteTagSource::Account`](crate::sync::NoteTagSource::Account) source makes
    ///   [`Store::apply_transaction`](crate::store::Store::apply_transaction) fail, because the
    ///   store derives these tags from the addresses of the native accounts.
    pub fn new(
        transaction: TransactionOutcome,
        submission_height: BlockNumber,
        note_updates: NoteUpdates,
        future_notes: Vec<(NoteDetails, NoteTag)>,
        new_tags: Vec<NoteTagRecord>,
    ) -> Self {
        Self {
            transaction,
            submission_height,
            future_notes,
            note_updates,
            new_tags,
        }
    }

    /// Returns the parts of the executed transaction that the store records.
    pub fn transaction(&self) -> &TransactionOutcome {
        &self.transaction
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
    pub fn note_updates(&self) -> &NoteUpdates {
        &self.note_updates
    }

    /// Returns the new tags that were created as part of the transaction.
    pub fn new_tags(&self) -> &[NoteTagRecord] {
        &self.new_tags
    }
}

// BATCH STORE UPDATE
// ================================================================================================

/// Represents the changes that need to be applied to the client store as a result of a submitted
/// transaction batch.
///
/// The store records each transaction and applies its account patch in batch order. The note
/// updates and the tags of all the transactions are merged, so the store writes each note once, in
/// its final in-batch state.
#[derive(Debug, Clone, PartialEq)]
pub struct BatchStoreUpdate {
    /// The executed transactions of the batch, in batch order.
    transactions: Vec<TransactionOutcome>,
    /// Block number at which the batch was submitted.
    submission_height: BlockNumber,
    /// Merged information about note changes after the execution of the transactions.
    note_updates: NoteUpdates,
    /// New note tags to be tracked.
    new_tags: Vec<NoteTagRecord>,
}

impl BatchStoreUpdate {
    /// Creates a new [`BatchStoreUpdate`] instance.
    pub fn new(
        transactions: Vec<TransactionOutcome>,
        submission_height: BlockNumber,
        note_updates: NoteUpdates,
        new_tags: Vec<NoteTagRecord>,
    ) -> Self {
        Self {
            transactions,
            submission_height,
            note_updates,
            new_tags,
        }
    }

    /// Returns the executed transactions of the batch, in batch order.
    pub fn transactions(&self) -> &[TransactionOutcome] {
        &self.transactions
    }

    /// Returns the block number at which the batch was submitted.
    pub fn submission_height(&self) -> BlockNumber {
        self.submission_height
    }

    /// Returns the merged note updates of the batch.
    pub fn note_updates(&self) -> &NoteUpdates {
        &self.note_updates
    }

    /// Returns the new tags that were created as part of the batch.
    pub fn new_tags(&self) -> &[NoteTagRecord] {
        &self.new_tags
    }
}

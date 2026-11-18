use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::Nullifier;
use miden_protocol::transaction::{TransactionHeader, TransactionId};

use super::TransactionUpdates;
use crate::transaction::{DiscardCause, TransactionRecord, TransactionStatus};

/// Contains transaction changes to apply to the store.
#[derive(Default)]
pub struct TransactionUpdateTracker {
    /// Transactions that were committed in the block.
    transactions: BTreeMap<TransactionId, TransactionRecord>,
    /// Nullifier-to-account mappings from external transactions by tracked accounts.
    external_nullifier_accounts: BTreeMap<Nullifier, AccountId>,
}

impl TransactionUpdateTracker {
    /// Creates a new [`TransactionUpdateTracker`]
    pub fn new(transactions: Vec<TransactionRecord>) -> Self {
        let transactions =
            transactions.into_iter().map(|tx| (tx.id, tx)).collect::<BTreeMap<_, _>>();

        Self {
            transactions,
            external_nullifier_accounts: BTreeMap::new(),
        }
    }

    /// Consumes the tracker and returns the transactions that need a store write.
    pub fn into_transaction_updates(self) -> TransactionUpdates {
        TransactionUpdates::new(self.transactions.into_values())
    }

    /// Returns a reference to committed transactions.
    pub fn committed_transactions(&self) -> impl Iterator<Item = &TransactionRecord> {
        self.transactions
            .values()
            .filter(|tx| matches!(tx.status, TransactionStatus::Committed { .. }))
    }

    /// Returns a reference to discarded transactions.
    pub fn discarded_transactions(&self) -> impl Iterator<Item = &TransactionRecord> {
        self.transactions
            .values()
            .filter(|tx| matches!(tx.status, TransactionStatus::Discarded(_)))
    }

    /// Returns a mutable reference to pending transactions in the tracker.
    fn mutable_pending_transactions(&mut self) -> impl Iterator<Item = &mut TransactionRecord> {
        self.transactions
            .values_mut()
            .filter(|tx| matches!(tx.status, TransactionStatus::Pending))
    }

    /// Returns transaction IDs of all transactions that have been updated.
    pub fn updated_transaction_ids(&self) -> impl Iterator<Item = TransactionId> {
        self.committed_transactions()
            .chain(self.discarded_transactions())
            .map(|tx| tx.id)
    }

    /// Returns the account ID that consumed the given nullifier in an external transaction, if
    /// available.
    pub fn external_nullifier_account(&self, nullifier: &Nullifier) -> Option<AccountId> {
        self.external_nullifier_accounts.get(nullifier).copied()
    }

    /// Applies the necessary state transitions to the [`TransactionUpdateTracker`] when a
    /// transaction is included in a block.
    ///
    /// The included transaction is matched to a local pending transaction by its ID only. The node
    /// reports the original transaction ID, so a record with an unknown ID is an external
    /// transaction of a tracked account.
    pub fn apply_transaction_inclusion(
        &mut self,
        block_num: BlockNumber,
        header: &TransactionHeader,
        timestamp: u64,
    ) {
        let account_id = header.account_id();

        if let Some(transaction) = self.transactions.get_mut(&header.id()) {
            transaction.commit_transaction(block_num, timestamp);
            return;
        }

        // No local transaction has this ID. This is an external transaction by a tracked account.
        // Record the nullifier→account mappings so we can attribute note consumption to tracked
        // accounts during nullifier processing.
        for commitment in header.input_notes().iter() {
            self.external_nullifier_accounts.insert(commitment.nullifier(), account_id);
        }
    }

    /// Applies the necessary state transitions to the [`TransactionUpdateTracker`] when a the sync
    /// height of the client is updated. This may result in stale or expired transactions.
    pub fn apply_sync_height_update(
        &mut self,
        new_sync_height: BlockNumber,
        tx_discard_delta: Option<u32>,
    ) {
        if let Some(tx_discard_delta) = tx_discard_delta {
            self.discard_transaction_with_predicate(
                |transaction| {
                    transaction.details.submission_height
                        < new_sync_height.checked_sub(tx_discard_delta).unwrap_or_default()
                },
                DiscardCause::Stale,
            );
        }

        // NOTE: we check for <= new_sync height because at this point we would have committed the
        // transaction otherwise
        self.discard_transaction_with_predicate(
            |transaction| transaction.details.expiration_block_num <= new_sync_height,
            DiscardCause::Expired,
        );
    }

    /// Applies the necessary state transitions to the [`TransactionUpdateTracker`] when a note is
    /// nullified. this may result in transactions being discarded because they were processing the
    /// nullified note.
    pub fn apply_input_note_nullified(&mut self, input_note_nullifier: Nullifier) {
        self.discard_transaction_with_predicate(
            |transaction| {
                // Check if the note was being processed by a local transaction that didn't end up
                // being committed so it should be discarded
                transaction
                    .details
                    .input_note_nullifiers
                    .contains(&input_note_nullifier.as_word())
            },
            DiscardCause::InputConsumed,
        );
    }

    /// Discards the local transaction that produced this now-superseded account state.
    pub fn apply_superseded_account_state(&mut self, superseded_account_state: Word) {
        self.discard_transaction_with_predicate(
            |transaction| transaction.details.final_account_state == superseded_account_state,
            DiscardCause::Superseded,
        );
    }

    /// Discards transactions that have the same initial account state as the provided one.
    pub fn apply_invalid_initial_account_state(&mut self, invalid_account_state: Word) {
        self.discard_transaction_with_predicate(
            |transaction| transaction.details.init_account_state == invalid_account_state,
            DiscardCause::DiscardedInitialState,
        );
    }

    /// Discards transactions that match the predicate and also applies the new invalid account
    /// states
    fn discard_transaction_with_predicate<F>(&mut self, predicate: F, discard_cause: DiscardCause)
    where
        F: Fn(&TransactionRecord) -> bool,
    {
        let mut new_invalid_account_states = vec![];

        for transaction in self.mutable_pending_transactions() {
            // Discard transactions, and also push the invalid account state if the transaction got
            // correctly discarded
            // NOTE: previous updates in a chain of state syncs could have committed a transaction,
            // so we need to check that `discard_transaction` returns `true` here (aka, it got
            // discarded from a valid state)
            if predicate(transaction) && transaction.discard_transaction(discard_cause) {
                new_invalid_account_states.push(transaction.details.final_account_state);
            }
        }

        for state in new_invalid_account_states {
            self.apply_invalid_initial_account_state(state);
        }
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::Felt;
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;
    use miden_protocol::transaction::{InputNoteCommitment, InputNotes, RawOutputNotes};

    use super::*;
    use crate::transaction::TransactionDetails;

    fn account_id() -> AccountId {
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap()
    }

    fn word(n: u64) -> Word {
        Word::from([
            Felt::new_unchecked(n),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
            Felt::new_unchecked(0),
        ])
    }

    // TRANSACTION INCLUSION TESTS
    // --------------------------------------------------------------------------------------------

    fn transaction_header(init_state: u64, final_state: u64, nullifier: u64) -> TransactionHeader {
        let input_notes = InputNotes::new_unchecked(vec![InputNoteCommitment::from(
            Nullifier::from_raw(word(nullifier)),
        )]);

        TransactionHeader::new(
            account_id(),
            word(init_state),
            word(final_state),
            input_notes,
            vec![],
        )
        .unwrap()
    }

    fn pending_transaction(init_state: u64, final_state: u64, nullifier: u64) -> TransactionRecord {
        let id = transaction_header(init_state, final_state, nullifier).id();
        let details = TransactionDetails {
            account_id: account_id(),
            init_account_state: word(init_state),
            final_account_state: word(final_state),
            input_note_nullifiers: vec![word(nullifier)],
            output_notes: RawOutputNotes::new(vec![]).unwrap(),
            block_num: BlockNumber::from(1u32),
            submission_height: BlockNumber::from(1u32),
            expiration_block_num: BlockNumber::from(100u32),
            creation_timestamp: 0,
        };

        TransactionRecord::new(id, details, None, TransactionStatus::Pending)
    }

    /// An included transaction with an unknown ID is external, even when it shares the account and
    /// the initial and final states with a local pending transaction.
    #[test]
    fn inclusion_with_unknown_id_does_not_commit_local_transaction() {
        let local = pending_transaction(10, 11, 1);
        let local_id = local.id;
        let mut tracker = TransactionUpdateTracker::new(vec![local]);

        let included = transaction_header(10, 11, 2);
        assert_ne!(included.id(), local_id);
        tracker.apply_transaction_inclusion(BlockNumber::from(5u32), &included, 0);

        assert!(tracker.committed_transactions().next().is_none());
        assert_eq!(
            tracker.external_nullifier_account(&Nullifier::from_raw(word(2))),
            Some(account_id())
        );
    }
}

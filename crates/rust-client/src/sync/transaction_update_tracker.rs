use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::Nullifier;
use miden_protocol::transaction::{TransactionHeader, TransactionId};

use super::TransactionUpdates;
use crate::transaction::{DiscardCause, TransactionRecord, TransactionStatus};

/// The pending local transactions of one account for this sync.
pub struct PendingAccountTransactions<'a> {
    account_id: AccountId,
    records: Vec<&'a mut TransactionRecord>,
}

impl PendingAccountTransactions<'_> {
    /// Returns the account that executed these transactions.
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    /// Discards transactions with a known cause and propagates each discard to dependent
    /// transactions. A state on the proven chain remains valid.
    ///
    /// Returns the account states that lost their pending producer. The account tracker selects the
    /// states on its local branch that need a rollback.
    pub fn resolve(
        mut self,
        established: &BTreeSet<Word>,
        cause: impl Fn(&TransactionRecord) -> Option<DiscardCause>,
    ) -> BTreeSet<Word> {
        let mut dependents: BTreeMap<Word, Vec<usize>> = BTreeMap::new();
        let mut discards = VecDeque::new();
        for (index, tx) in self.records.iter().enumerate() {
            dependents.entry(tx.details.init_account_state).or_default().push(index);
            if let Some(cause) = cause(tx) {
                discards.push_back((index, cause));
            }
        }

        let mut invalid_states = BTreeSet::new();
        while let Some((index, cause)) = discards.pop_front() {
            let tx = &mut *self.records[index];
            let state = tx.details.final_account_state;
            if !tx.discard_transaction(cause)
                || state == tx.details.init_account_state
                || established.contains(&state)
                || !invalid_states.insert(state)
            {
                continue;
            }

            if let Some(indices) = dependents.remove(&state) {
                discards
                    .extend(indices.into_iter().map(|i| (i, DiscardCause::DiscardedInitialState)));
            }
        }
        invalid_states
    }
}

/// Tracks local transactions and external note consumers during one sync.
#[derive(Default)]
pub struct TransactionUpdateTracker {
    /// Local transaction records indexed by ID.
    transactions: BTreeMap<TransactionId, TransactionRecord>,
    /// Nullifier-to-account mappings from external transactions by tracked accounts.
    external_nullifier_accounts: BTreeMap<Nullifier, AccountId>,
}

impl TransactionUpdateTracker {
    /// Creates a new [`TransactionUpdateTracker`]
    pub fn new(transactions: Vec<TransactionRecord>) -> Self {
        let transactions: BTreeMap<_, _> = transactions.into_iter().map(|tx| (tx.id, tx)).collect();
        Self {
            transactions,
            external_nullifier_accounts: BTreeMap::new(),
        }
    }

    /// Returns the local transactions that still need an inclusion or a discard decision.
    pub fn pending_transactions(&self) -> impl Iterator<Item = &TransactionRecord> {
        self.transactions
            .values()
            .filter(|tx| matches!(tx.status, TransactionStatus::Pending))
    }

    /// Consumes the tracker and returns the transaction records that need a store write.
    pub fn into_transaction_updates(self) -> TransactionUpdates {
        TransactionUpdates::new(self.transactions.into_values())
    }

    /// Lends each account its pending transactions for reconciliation.
    pub fn pending_by_account(&mut self) -> impl Iterator<Item = PendingAccountTransactions<'_>> {
        let mut by_account: BTreeMap<AccountId, Vec<&mut TransactionRecord>> = BTreeMap::new();
        for tx in self
            .transactions
            .values_mut()
            .filter(|tx| matches!(tx.status, TransactionStatus::Pending))
        {
            by_account.entry(tx.details.account_id).or_default().push(tx);
        }
        by_account
            .into_iter()
            .map(|(account_id, records)| PendingAccountTransactions { account_id, records })
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
    ///
    /// Apply every inclusion of the synced range before [`Self::pending_by_account`]. A discard is
    /// terminal, so a local transaction that the node included must be committed before a competing
    /// transition can discard it.
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

    /// Discards transactions that are stale or expired and returns the invalidated account states.
    pub fn apply_sync_height_update(
        &mut self,
        height: BlockNumber,
        tx_discard_delta: Option<u32>,
    ) -> Vec<(AccountId, Word)> {
        let stale_before =
            tx_discard_delta.and_then(|delta| height.checked_sub(delta)).unwrap_or_default();
        self.discard_where(|tx| tx.details.discard_cause_at(height, stale_before))
    }

    /// Discards transactions that need the spent input and returns the invalidated account states.
    pub fn apply_input_note_nullified(&mut self, nullifier: Nullifier) -> Vec<(AccountId, Word)> {
        self.discard_where(|tx| {
            tx.details
                .input_note_nullifiers
                .contains(&nullifier.as_word())
                .then_some(DiscardCause::InputConsumed)
        })
    }

    /// Discards pending transactions that produced or require the superseded state.
    pub fn apply_superseded_account_state(&mut self, state: Word) -> Vec<(AccountId, Word)> {
        self.discard_where(|tx| {
            if tx.details.final_account_state == state {
                Some(DiscardCause::Superseded)
            } else {
                (tx.details.init_account_state == state)
                    .then_some(DiscardCause::DiscardedInitialState)
            }
        })
    }

    /// Discards pending transactions that require the invalid state.
    pub fn apply_invalid_initial_account_state(&mut self, state: Word) -> Vec<(AccountId, Word)> {
        self.discard_where(|tx| {
            (tx.details.init_account_state == state).then_some(DiscardCause::DiscardedInitialState)
        })
    }

    fn discard_where(
        &mut self,
        cause: impl Fn(&TransactionRecord) -> Option<DiscardCause>,
    ) -> Vec<(AccountId, Word)> {
        self.pending_by_account()
            .flat_map(|pending| {
                let account_id = pending.account_id();
                pending
                    .resolve(&BTreeSet::new(), &cause)
                    .into_iter()
                    .map(move |state| (account_id, state))
            })
            .collect()
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

    fn status(tracker: &TransactionUpdateTracker, id: TransactionId) -> TransactionStatus {
        tracker.transactions.get(&id).unwrap().status.clone()
    }

    #[test]
    fn inclusion_commits_each_passthrough_by_id_before_reconciliation() {
        for reverse in [false, true] {
            let mut headers = vec![
                transaction_header(1, 1, 1),
                transaction_header(1, 1, 2),
                transaction_header(1, 2, 3),
            ];
            let mut tracker = TransactionUpdateTracker::new(vec![
                pending_transaction(1, 1, 1),
                pending_transaction(1, 1, 2),
                pending_transaction(1, 2, 3),
            ]);
            if reverse {
                headers.reverse();
            }
            for header in &headers {
                tracker.apply_transaction_inclusion(BlockNumber::from(5u32), header, 0);
            }
            assert_eq!(tracker.pending_by_account().count(), 0);
            for header in headers {
                assert!(matches!(
                    status(&tracker, header.id()),
                    TransactionStatus::Committed { .. }
                ));
            }
        }
    }

    /// A discarded transaction that left the account state unchanged does not invalidate that
    /// state. The other pending transactions from the state stay valid.
    #[test]
    fn discarded_state_preserving_transaction_keeps_its_state_valid() {
        let state_preserving = pending_transaction(1, 1, 1);
        let state_preserving_id = state_preserving.id;
        let from_same_state = pending_transaction(1, 2, 2);
        let from_same_state_id = from_same_state.id;
        let mut tracker = TransactionUpdateTracker::new(vec![state_preserving, from_same_state]);

        // The input note of the state-preserving transaction is consumed elsewhere.
        tracker.apply_input_note_nullified(Nullifier::from_raw(word(1)));

        assert_eq!(
            status(&tracker, state_preserving_id),
            TransactionStatus::Discarded(DiscardCause::InputConsumed)
        );
        assert_eq!(status(&tracker, from_same_state_id), TransactionStatus::Pending);
    }

    /// A superseded state invalidates the transactions that start from it, also when only a
    /// state-preserving transaction ends in it.
    #[test]
    fn superseded_state_discards_the_transactions_that_start_from_it() {
        let state_preserving = pending_transaction(1, 1, 1);
        let state_preserving_id = state_preserving.id;
        let from_superseded = pending_transaction(1, 2, 2);
        let from_superseded_id = from_superseded.id;
        let mut tracker = TransactionUpdateTracker::new(vec![state_preserving, from_superseded]);

        tracker.apply_superseded_account_state(word(1));

        assert_eq!(
            status(&tracker, state_preserving_id),
            TransactionStatus::Discarded(DiscardCause::Superseded)
        );
        assert_eq!(
            status(&tracker, from_superseded_id),
            TransactionStatus::Discarded(DiscardCause::DiscardedInitialState)
        );
    }
}

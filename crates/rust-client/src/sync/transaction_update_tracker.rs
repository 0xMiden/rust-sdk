use alloc::collections::{BTreeMap, BTreeSet};
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

    /// Discards each transaction that has a direct cause and preserves that cause. A dependent
    /// transaction is discarded if it needs an account state that a discard invalidates.
    ///
    /// Returns the account states invalidated by these discards.
    pub fn discard_where(
        self,
        confirmed_states: &BTreeSet<Word>,
        discard_cause: impl Fn(&TransactionRecord) -> Option<DiscardCause>,
    ) -> BTreeSet<Word> {
        let mut to_discard = Vec::new();
        let mut pending_by_initial_state: BTreeMap<Word, Vec<_>> = BTreeMap::new();
        for tx in self.records {
            if let Some(cause) = discard_cause(tx) {
                to_discard.push((tx, cause));
            } else {
                pending_by_initial_state
                    .entry(tx.details.init_account_state)
                    .or_default()
                    .push(tx);
            }
        }

        let mut invalid_states = BTreeSet::new();
        while let Some((tx, cause)) = to_discard.pop() {
            tx.discard_transaction(cause);

            let final_state = tx.details.final_account_state;
            if final_state == tx.details.init_account_state
                || confirmed_states.contains(&final_state)
            {
                continue;
            }
            invalid_states.insert(final_state);
            if let Some(dependents) = pending_by_initial_state.remove(&final_state) {
                for dependent in dependents {
                    to_discard.push((dependent, DiscardCause::DiscardedInitialState));
                }
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

    #[test]
    fn dependent_transactions_keep_their_own_discard_cause() {
        let parent = pending_transaction(1, 2, 1);
        let child = pending_transaction(2, 3, 2);
        let grandchild = pending_transaction(3, 4, 3);
        let (parent_id, child_id, grandchild_id) = (parent.id, child.id, grandchild.id);
        let mut tracker = TransactionUpdateTracker::new(vec![grandchild, child, parent]);
        let pending = tracker.pending_by_account().next().unwrap();

        let invalid_states = pending.discard_where(&BTreeSet::new(), |tx| {
            if tx.id == parent_id {
                Some(DiscardCause::Expired)
            } else if tx.id == child_id {
                Some(DiscardCause::InputConsumed)
            } else {
                None
            }
        });

        assert_eq!(
            status(&tracker, child_id),
            TransactionStatus::Discarded(DiscardCause::InputConsumed)
        );
        assert_eq!(
            status(&tracker, grandchild_id),
            TransactionStatus::Discarded(DiscardCause::DiscardedInitialState)
        );
        assert_eq!(invalid_states, BTreeSet::from([word(2), word(3), word(4)]));
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

        let pending = tracker.pending_by_account().next().unwrap();
        let invalid_states = pending.discard_where(&BTreeSet::new(), |tx| {
            (tx.id == state_preserving_id).then_some(DiscardCause::InputConsumed)
        });
        assert!(invalid_states.is_empty());

        assert_eq!(
            status(&tracker, state_preserving_id),
            TransactionStatus::Discarded(DiscardCause::InputConsumed)
        );
        assert_eq!(status(&tracker, from_same_state_id), TransactionStatus::Pending);
    }
}

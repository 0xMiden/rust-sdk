use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cmp::Ordering;

use miden_protocol::Word;
use miden_protocol::account::{AccountHeader, AccountId};
use miden_protocol::block::BlockNumber;
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::transaction::TransactionHeader;

use super::{AccountUpdates, PendingAccountTransactions, PublicAccountUpdate};
use crate::store::{AccountStatus, StoreError};
use crate::transaction::{DiscardCause, TransactionRecord, TransactionStatus};

// PUBLIC ACCOUNT RECONCILIATION
// ================================================================================================

/// The effect that the network state of a public account has on the local state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PublicAccountReconciliation {
    /// The local state stays. The network state is equal to the local state, or the node did not
    /// include every local pending transaction yet.
    Keep,
    /// The network state is a later state of the confirmed local state. It replaces the local
    /// state.
    Apply,
    /// The network state is on a different branch than the local state. It replaces the local
    /// state, and the local pending transactions that produced the local state lost the race.
    Supersede,
}

// TRACKED ACCOUNT
// ================================================================================================

/// The result of the reconciliation of one tracked account.
#[derive(Clone, Debug)]
pub(super) enum AccountSyncState {
    /// The sync has no network state that differs from the local state.
    Unchanged,
    /// The node reported a commitment that differs from the local commitment. The sync must fetch
    /// the account to authenticate the network state. The local state stays unless the fetched
    /// state replaces it.
    Diverging,
    /// The network state of a public account replaces the confirmed local state.
    Updated(Box<PublicAccountUpdate>),
    /// The network state of a public account replaces a local state on a different branch.
    Superseded(Box<PublicAccountUpdate>),
    /// The proven private state is unknown and the account must become locked.
    Lock,
}

impl PublicAccountReconciliation {
    /// Attaches the fetched account data to the decision made from the proven header.
    pub(super) fn with_update(self, update: PublicAccountUpdate) -> AccountSyncState {
        match self {
            Self::Keep => AccountSyncState::Unchanged,
            Self::Apply => AccountSyncState::Updated(Box::new(update)),
            Self::Supersede => AccountSyncState::Superseded(Box::new(update)),
        }
    }
}

/// An account held by the [`AccountUpdateTracker`].
#[derive(Clone, Debug)]
pub(super) struct TrackedAccount {
    /// Header of the account state that the store holds.
    pub(super) local_header: AccountHeader,
    already_locked: bool,
    /// Result of the reconciliation of the network state with the local state.
    state: AccountSyncState,
    /// State-changing local transactions indexed by their final commitment.
    local_transitions: AccountTransitions,
    /// State-changing transactions reported in the sync range, indexed by their final commitment.
    reported_transitions: AccountTransitions,
    /// Account commitment proven at the target block.
    proven_state: Option<Word>,
}

/// State-changing transactions of an account, indexed by their final commitment.
#[derive(Clone, Debug, Default)]
struct AccountTransitions(BTreeMap<Word, Word>);

impl AccountTransitions {
    fn insert(&mut self, initial: Word, result: Word) {
        if initial != result {
            self.0.insert(result, initial);
        }
    }

    /// Walks backwards from the tip and yields each (result, initial) pair once.
    fn branch(&self, mut tip: Word) -> impl Iterator<Item = (Word, Word)> + '_ {
        let mut seen = BTreeSet::new();
        core::iter::from_fn(move || {
            let result = tip;
            let initial = *self.0.get(&result)?;
            if !seen.insert(result) {
                return None;
            }
            tip = initial;
            Some((result, initial))
        })
    }
}

/// Discard conditions that apply across accounts in one sync range.
#[derive(Default)]
pub(super) struct DiscardConditions {
    pub(super) height: BlockNumber,
    pub(super) stale_before: BlockNumber,
    pub(super) spent_inputs: BTreeSet<Word>,
}

impl DiscardConditions {
    fn cause(&self, tx: &TransactionRecord) -> Option<DiscardCause> {
        let details = &tx.details;
        details.discard_cause_at(self.height, self.stale_before).or_else(|| {
            details
                .input_note_nullifiers
                .iter()
                .any(|n| self.spent_inputs.contains(n))
                .then_some(DiscardCause::InputConsumed)
        })
    }
}

impl TrackedAccount {
    /// Compares the network state of a public account with its local state.
    ///
    /// The caller must authenticate `network_header` against the sync target first. The proof must
    /// belong to this account.
    pub(super) fn reconcile_public_account(
        &self,
        network_header: &AccountHeader,
    ) -> PublicAccountReconciliation {
        let local_state = self.local_header.to_commitment();
        let network_state = network_header.to_commitment();
        if network_state == local_state {
            return PublicAccountReconciliation::Keep;
        }
        let mut pending_branch = self.local_transitions.branch(local_state).peekable();
        if pending_branch.peek().is_some() {
            return if pending_branch.any(|(_, initial)| initial == network_state) {
                PublicAccountReconciliation::Keep
            } else {
                PublicAccountReconciliation::Supersede
            };
        }

        // The local state is confirmed. Two different states with the same nonce are on different
        // branches. A confirmed state is on chain, so a network state with a lower nonce is not
        // expected. The local state stays in that case.
        let network_nonce = network_header.nonce().as_canonical_u64();
        let local_nonce = self.local_header.nonce().as_canonical_u64();
        match network_nonce.cmp(&local_nonce) {
            Ordering::Less => PublicAccountReconciliation::Keep,
            Ordering::Equal => PublicAccountReconciliation::Supersede,
            Ordering::Greater => PublicAccountReconciliation::Apply,
        }
    }

    /// Resolves pending transactions against the proven state and returns the local states to undo.
    fn reconcile(
        &self,
        pending: PendingAccountTransactions,
        conditions: &DiscardConditions,
    ) -> Vec<Word> {
        let mut established = BTreeSet::new();
        if let Some(tip) = self.proven_state {
            established.insert(tip);
            established.extend(self.reported_transitions.branch(tip).map(|(_, initial)| initial));
        }

        // Only the local states above the last confirmed state can be rolled back. Undoing an older
        // state would remove the confirmed tip from the store.
        let tip = self.local_header.to_commitment();
        let mut speculative: Vec<_> = self
            .local_transitions
            .branch(tip)
            .map(|(result, _)| result)
            .take_while(|result| !established.contains(result))
            .collect();
        let superseded =
            matches!(self.state, AccountSyncState::Superseded(_)) && !established.contains(&tip);
        let first_losing_state = speculative.last().copied().unwrap_or(tip);
        let invalid = pending.discard_where(&established, |tx| {
            let initial = tx.details.init_account_state;
            let network_advanced =
                established.contains(&initial) && self.proven_state != Some(initial);
            if network_advanced
                || (superseded && tx.details.final_account_state == first_losing_state)
            {
                Some(DiscardCause::Superseded)
            } else if superseded && initial == tip {
                Some(DiscardCause::DiscardedInitialState)
            } else {
                conditions.cause(tx)
            }
        });
        speculative.retain(|state| invalid.contains(state));
        speculative
    }
}

// ACCOUNT UPDATE TRACKER
// ================================================================================================

/// Tracks the account changes that a sync derives, before they become [`AccountUpdates`].
///
/// The tracker holds the header of each account that the client tracks, and the local pending
/// transactions that lead to it. The sync applies the account states that the network reports. The
/// tracker gives the accounts that the sync must fetch, and it decides how each fetched state
/// changes the local state. It holds at most one change for each account.
///
/// The tracker does not fetch data and does not write to the store.
#[derive(Clone, Debug)]
pub(crate) struct AccountUpdateTracker {
    /// The tracked accounts, keyed by account ID.
    accounts: BTreeMap<AccountId, TrackedAccount>,
    states_to_undo: Vec<(AccountId, Word)>,
    /// Witnesses that the sync validated at its target block, keyed by account ID. The client also
    /// keeps the witnesses of accounts that it does not track, so an entry can have no account in
    /// `accounts`.
    account_witnesses: BTreeMap<AccountId, AccountWitness>,
}

impl AccountUpdateTracker {
    /// Creates an [`AccountUpdateTracker`] with the headers of the tracked accounts and the local
    /// transactions that are not committed yet.
    pub(crate) fn new(
        accounts: impl IntoIterator<Item = (AccountHeader, AccountStatus)>,
        uncommitted_transactions: &[TransactionRecord],
    ) -> Self {
        let mut accounts: BTreeMap<_, _> = accounts
            .into_iter()
            .map(|(local_header, status)| {
                let account = TrackedAccount {
                    local_header,
                    already_locked: status.is_locked(),
                    state: AccountSyncState::Unchanged,
                    local_transitions: AccountTransitions::default(),
                    reported_transitions: AccountTransitions::default(),
                    proven_state: None,
                };
                (account.local_header.id(), account)
            })
            .collect();

        for tx in uncommitted_transactions {
            let details = &tx.details;
            if matches!(tx.status, TransactionStatus::Pending)
                && let Some(account) = accounts.get_mut(&details.account_id)
            {
                account
                    .local_transitions
                    .insert(details.init_account_state, details.final_account_state);
            }
        }

        Self {
            accounts,
            states_to_undo: Vec::new(),
            account_witnesses: BTreeMap::new(),
        }
    }

    /// Consumes the tracker and returns the account changes that need a store write.
    pub(crate) fn into_account_updates(self) -> AccountUpdates {
        let mut updated_public_accounts = Vec::new();
        let mut accounts_to_lock = Vec::new();

        for (account_id, account) in self.accounts {
            match account.state {
                AccountSyncState::Unchanged | AccountSyncState::Diverging => {},
                AccountSyncState::Updated(update) | AccountSyncState::Superseded(update) => {
                    updated_public_accounts.push(*update);
                },
                AccountSyncState::Lock => {
                    accounts_to_lock.push(account_id);
                },
            }
        }

        AccountUpdates::new(updated_public_accounts, accounts_to_lock)
            .with_states_to_undo(self.states_to_undo)
            .with_account_witnesses(self.account_witnesses.into_iter().collect())
    }

    // GETTERS
    // --------------------------------------------------------------------------------------------

    /// Returns the tracked public accounts that the sync must fetch.
    ///
    /// Public accounts and network accounts keep their state on chain and follow the same sync
    /// path. Both are public accounts here.
    pub(super) fn diverging_public_accounts(&self) -> impl Iterator<Item = &TrackedAccount> {
        self.diverging_accounts()
            .filter(|account| !account.local_header.id().is_private())
    }

    /// Returns the headers of the private accounts whose on-chain commitment the sync must fetch.
    pub(crate) fn diverging_private_accounts(&self) -> impl Iterator<Item = &AccountHeader> {
        self.diverging_accounts()
            .map(|account| &account.local_header)
            .filter(|header| header.id().is_private())
    }

    /// Reconciles one account's pending transactions and records the local states to undo.
    pub(super) fn reconcile_pending(
        &mut self,
        pending: PendingAccountTransactions,
        conditions: &DiscardConditions,
    ) {
        let id = pending.account_id();
        let states = match self.accounts.get(&id) {
            Some(account) => account.reconcile(pending, conditions),
            None => pending
                .discard_where(&BTreeSet::new(), |tx| conditions.cause(tx))
                .into_iter()
                .collect(),
        };
        self.states_to_undo.extend(states.into_iter().map(|state| (id, state)));
    }

    /// Requests a proof before a reported transition can discard a pending transaction.
    pub(super) fn observe_transition(&mut self, header: &TransactionHeader) {
        let initial = header.initial_state_commitment();
        let final_state = header.final_state_commitment();
        if initial != final_state
            && let Some(account) = self.accounts.get_mut(&header.account_id())
        {
            account.reported_transitions.insert(initial, final_state);
            account.state = AccountSyncState::Diverging;
        }
    }

    /// Returns whether the tracker holds a witness of the account.
    pub(crate) fn has_account_witness(&self, account_id: AccountId) -> bool {
        self.account_witnesses.contains_key(&account_id)
    }

    /// Returns the accounts whose reported network state still needs a proof.
    fn diverging_accounts(&self) -> impl Iterator<Item = &TrackedAccount> {
        self.accounts
            .values()
            .filter(|account| matches!(account.state, AccountSyncState::Diverging))
    }

    // UPDATES
    // --------------------------------------------------------------------------------------------

    /// Applies an account commitment that the node reported for the synced block range.
    ///
    /// The commitment is not authenticated. A commitment that differs from the local commitment
    /// only marks the account for a fetch. A commitment of an account that the tracker does not
    /// hold has no effect.
    pub(crate) fn apply_commitment_update(&mut self, account_id: AccountId, commitment: Word) {
        let Some(account) = self.accounts.get_mut(&account_id) else {
            return;
        };

        if matches!(account.state, AccountSyncState::Unchanged)
            && commitment != account.local_header.to_commitment()
        {
            account.state = AccountSyncState::Diverging;
        }
    }

    /// Records the result of a public account fetch and its proof at the sync target.
    pub(super) fn record_public_account(
        &mut self,
        witness: AccountWitness,
        state: AccountSyncState,
    ) {
        let id = witness.id();
        if let Some(account) = self.accounts.get_mut(&id) {
            account.state = state;
        }
        self.insert_account_witness(id, witness);
    }

    /// Records the proven private state and decides whether the account needs a new lock.
    pub(super) async fn apply_private_account_commitment(
        &mut self,
        id: AccountId,
        commitment: Word,
        is_known_private_state: impl AsyncFnOnce(AccountId, Word) -> Result<bool, StoreError>,
    ) -> Result<(), StoreError> {
        let Some(account) = self.accounts.get_mut(&id) else {
            return Ok(());
        };
        let tip = account.local_header.to_commitment();
        account.state = if account.already_locked
            || tip == commitment
            || account.local_transitions.branch(tip).any(|(_, initial)| initial == commitment)
            || is_known_private_state(id, commitment).await?
        {
            AccountSyncState::Unchanged
        } else {
            AccountSyncState::Lock
        };
        account.proven_state = Some(commitment);
        Ok(())
    }

    /// Inserts an account witness that the sync validated at its target block. The witness replaces
    /// an earlier witness of the same account.
    pub(crate) fn insert_account_witness(
        &mut self,
        account_id: AccountId,
        witness: AccountWitness,
    ) {
        if let Some(account) = self.accounts.get_mut(&account_id) {
            account.proven_state = Some(witness.state_commitment());
        }
        self.account_witnesses.insert(account_id, witness);
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use alloc::vec::Vec;

    use miden_protocol::account::{AccountCode, AccountVaultPatch};
    use miden_protocol::block::BlockNumber;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
    };
    use miden_protocol::transaction::{RawOutputNotes, TransactionId};
    use miden_protocol::{EMPTY_WORD, Felt};

    use super::*;
    use crate::sync::TransactionUpdateTracker;
    use crate::sync::state_sync_update::build_account_patch;
    use crate::transaction::TransactionDetails;

    // HELPERS
    // --------------------------------------------------------------------------------------------

    fn public_account_id() -> AccountId {
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap()
    }

    fn private_account_id() -> AccountId {
        ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE.try_into().unwrap()
    }

    fn word(n: u32) -> Word {
        Word::from([n, 0, 0, 0])
    }

    /// Returns an account header. Headers with different `state` values have different commitments.
    fn header(account_id: AccountId, nonce: u32, state: u32) -> AccountHeader {
        AccountHeader::new(account_id, Felt::from(nonce), word(state), EMPTY_WORD, EMPTY_WORD)
    }

    /// Returns an update that advances a public account to `new_header`.
    fn patch_update(new_header: &AccountHeader) -> PublicAccountUpdate {
        let patch = build_account_patch(
            new_header,
            Vec::new(),
            BTreeMap::new(),
            AccountVaultPatch::default(),
            AccountCode::mock(),
            Some(AccountCode::mock().commitment()),
        )
        .unwrap();

        PublicAccountUpdate::Patch { new_header: new_header.clone(), patch }
    }

    fn ids<'a>(headers: impl Iterator<Item = &'a AccountHeader>) -> Vec<AccountId> {
        headers.map(AccountHeader::id).collect()
    }

    /// Returns a pending local transaction of the public account from `initial` to `final_state`.
    fn pending(initial: &AccountHeader, final_state: &AccountHeader) -> TransactionRecord {
        let initial_commitment = initial.to_commitment();
        let final_commitment = final_state.to_commitment();
        TransactionRecord::new(
            TransactionId::from_raw(final_commitment),
            TransactionDetails {
                account_id: initial.id(),
                init_account_state: initial_commitment,
                final_account_state: final_commitment,
                input_note_nullifiers: Vec::new(),
                output_notes: RawOutputNotes::new(Vec::new()).unwrap(),
                block_num: BlockNumber::GENESIS,
                submission_height: BlockNumber::GENESIS,
                expiration_block_num: BlockNumber::from(1000u32),
                creation_timestamp: 0,
            },
            None,
            TransactionStatus::Pending,
        )
    }

    // RECONCILIATION TESTS
    // --------------------------------------------------------------------------------------------

    /// Without pending transactions, the local state is confirmed and the nonces decide.
    #[test]
    fn reconciliation_of_a_confirmed_state_compares_nonces() {
        let local = header(public_account_id(), 2, 2);
        let tracker = AccountUpdateTracker::new([(local.clone(), AccountStatus::Tracked)], &[]);

        let behind = header(public_account_id(), 1, 1);
        let fork = header(public_account_id(), 2, 9);
        let ahead = header(public_account_id(), 3, 3);

        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&local),
            PublicAccountReconciliation::Keep
        );
        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&behind),
            PublicAccountReconciliation::Keep
        );
        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&fork),
            PublicAccountReconciliation::Supersede
        );
        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&ahead),
            PublicAccountReconciliation::Apply
        );
    }

    /// With pending transactions, a network state on the way to the local state means that the node
    /// did not include every local pending transaction yet. Any other network state is on another
    /// branch and supersedes the local state, also when the local branch is longer.
    #[test]
    fn reconciliation_of_a_pending_state_follows_the_local_branch() {
        let confirmed = header(public_account_id(), 1, 1);
        let intermediate = header(public_account_id(), 2, 2);
        let local = header(public_account_id(), 3, 3);
        let tracker = AccountUpdateTracker::new(
            [(local.clone(), AccountStatus::Tracked)],
            &[pending(&confirmed, &intermediate), pending(&intermediate, &local)],
        );

        let shorter_branch = header(public_account_id(), 2, 9);
        let longer_branch = header(public_account_id(), 4, 9);

        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&confirmed),
            PublicAccountReconciliation::Keep
        );
        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&intermediate),
            PublicAccountReconciliation::Keep
        );
        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&shorter_branch),
            PublicAccountReconciliation::Supersede
        );
        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&longer_branch),
            PublicAccountReconciliation::Supersede
        );
    }

    /// A pending transaction that leaves the account state unchanged does not make the local state
    /// a pending state.
    #[test]
    fn reconciliation_ignores_state_preserving_pending_transactions() {
        let local = header(public_account_id(), 2, 2);
        let tracker = AccountUpdateTracker::new(
            [(local.clone(), AccountStatus::Tracked)],
            &[pending(&local, &local)],
        );

        let ahead = header(public_account_id(), 3, 3);
        assert_eq!(
            tracker.accounts[&local.id()].reconcile_public_account(&ahead),
            PublicAccountReconciliation::Apply
        );
    }

    async fn reconcile_private(
        local: &AccountHeader,
        proven: &AccountHeader,
        records: &[TransactionRecord],
        transitions: &[(&AccountHeader, &AccountHeader)],
    ) -> crate::sync::StateSyncUpdate {
        let mut accounts =
            AccountUpdateTracker::new([(local.clone(), AccountStatus::Tracked)], records);
        let mut transactions = TransactionUpdateTracker::new(records.to_vec());
        for (initial, result) in transitions {
            let header = TransactionHeader::new(
                local.id(),
                initial.to_commitment(),
                result.to_commitment(),
                miden_protocol::transaction::InputNotes::new_unchecked(vec![]),
                vec![],
            )
            .unwrap();
            transactions.apply_transaction_inclusion(BlockNumber::from(1u32), &header, 0);
            accounts.observe_transition(&header);
        }
        accounts
            .apply_private_account_commitment(local.id(), proven.to_commitment(), async |_, _| {
                Ok(false)
            })
            .await
            .unwrap();
        for pending in transactions.pending_by_account() {
            accounts.reconcile_pending(pending, &DiscardConditions::default());
        }
        crate::sync::StateSyncUpdate::from_parts(
            BlockNumber::from(1u32),
            crate::sync::PartialBlockchainUpdates::default(),
            miden_client_core::store::NoteUpdates::default(),
            transactions.into_transaction_updates(),
            accounts.into_account_updates(),
            None,
        )
    }

    #[tokio::test]
    async fn reconverging_branches_keep_the_confirmed_state_and_its_pending_child() {
        let [base, losing, shared, child] =
            [1, 2, 3, 4].map(|n| header(private_account_id(), n, n));
        let winner = header(private_account_id(), 2, 9);
        let records =
            [pending(&base, &losing), pending(&losing, &shared), pending(&shared, &child)];
        let update =
            reconcile_private(&child, &shared, &records, &[(&base, &winner), (&winner, &shared)])
                .await;
        let decisions: BTreeMap<_, _> = update
            .transaction_updates()
            .discarded_transactions()
            .map(|tx| (tx.id, &tx.status))
            .collect();
        assert_eq!(decisions.len(), 2);
        assert_eq!(
            decisions[&records[0].id],
            &TransactionStatus::Discarded(DiscardCause::Superseded)
        );
        assert_eq!(
            decisions[&records[1].id],
            &TransactionStatus::Discarded(DiscardCause::DiscardedInitialState)
        );
        assert!(
            !update
                .transaction_updates()
                .updated_transaction_ids()
                .any(|id| id == records[2].id)
        );
        assert!(update.account_updates().states_to_undo().is_empty());
    }

    #[tokio::test]
    async fn a_transition_outside_the_proven_chain_keeps_the_pending_transaction() {
        let [a, b, x] = [1, 2, 3].map(|n| header(private_account_id(), n, n));
        let update = reconcile_private(&b, &a, &[pending(&a, &b)], &[(&a, &x)]).await;
        assert_eq!(update.transaction_updates().updated_transaction_ids().count(), 0);
        assert!(update.account_updates().states_to_undo().is_empty());
    }

    #[tokio::test]
    async fn an_external_passthrough_keeps_an_independent_pending_transaction() {
        let [a, b] = [1, 2].map(|n| header(private_account_id(), n, n));
        let update = reconcile_private(&b, &a, &[pending(&a, &b)], &[(&a, &a)]).await;
        assert_eq!(update.transaction_updates().updated_transaction_ids().count(), 0);
        assert!(update.account_updates().states_to_undo().is_empty());
    }

    #[tokio::test]
    async fn sync_summary_reports_only_new_private_locks() {
        let local = header(private_account_id(), 2, 2);
        let ancestor = header(local.id(), 1, 1);
        let archived = header(local.id(), 1, 2).to_commitment();
        let unfamiliar = header(local.id(), 2, 9).to_commitment();
        for (status, proven, expect_lookup, expected) in [
            (AccountStatus::Tracked, local.to_commitment(), false, vec![]),
            (AccountStatus::Tracked, ancestor.to_commitment(), false, vec![]),
            (AccountStatus::Tracked, archived, true, vec![]),
            (AccountStatus::Tracked, unfamiliar, true, vec![local.id()]),
            (AccountStatus::Locked { seed: None }, unfamiliar, false, vec![]),
        ] {
            let mut accounts =
                AccountUpdateTracker::new([(local.clone(), status)], &[pending(&ancestor, &local)]);
            let mut lookup = None;
            accounts
                .apply_private_account_commitment(local.id(), proven, async |id, commitment| {
                    lookup = Some((id, commitment));
                    Ok(commitment == archived)
                })
                .await
                .unwrap();
            assert_eq!(lookup, expect_lookup.then_some((local.id(), proven)));
            let update = crate::sync::StateSyncUpdate::from_parts(
                BlockNumber::from(1u32),
                crate::sync::PartialBlockchainUpdates::default(),
                miden_client_core::store::NoteUpdates::default(),
                crate::sync::TransactionUpdates::default(),
                accounts.into_account_updates(),
                None,
            );
            assert_eq!(update.account_updates().accounts_to_lock(), expected);
            assert_eq!(crate::sync::SyncSummary::from(&update).locked_accounts, expected);
        }
    }

    // TRACKER TESTS
    // --------------------------------------------------------------------------------------------

    #[test]
    fn commitment_update_marks_only_a_different_commitment_for_a_fetch() {
        let public = header(public_account_id(), 1, 1);
        let private = header(private_account_id(), 1, 1);
        let faucet = header(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET.try_into().unwrap(), 1, 1);
        let mut tracker = AccountUpdateTracker::new(
            [public.clone(), private.clone(), faucet.clone()].map(|h| (h, AccountStatus::Tracked)),
            &[],
        );

        tracker.apply_commitment_update(public.id(), word(100));
        tracker.apply_commitment_update(private.id(), word(100));
        // The node reports the commitment that the client already holds.
        tracker.apply_commitment_update(faucet.id(), faucet.to_commitment());

        assert_eq!(
            ids(tracker.diverging_public_accounts().map(|account| &account.local_header)),
            vec![public.id()]
        );
        assert_eq!(ids(tracker.diverging_private_accounts()), vec![private.id()]);

        // An account without an authenticated network state has no change to write.
        let updates = tracker.into_account_updates();
        assert!(updates.updated_public_accounts().is_empty());
        assert!(updates.accounts_to_lock().is_empty());
    }

    #[test]
    fn superseded_account_discards_pending_transactions_from_its_losing_state() {
        let local = header(public_account_id(), 2, 2);
        let winner = header(local.id(), 2, 9);
        let child = header(local.id(), 3, 3);
        let passthrough = pending(&local, &local);
        let dependent = pending(&local, &child);
        let expected = BTreeMap::from([
            (passthrough.id, TransactionStatus::Discarded(DiscardCause::Superseded)),
            (dependent.id, TransactionStatus::Discarded(DiscardCause::DiscardedInitialState)),
        ]);
        let records = vec![passthrough, dependent];
        let mut accounts =
            AccountUpdateTracker::new([(local.clone(), AccountStatus::Tracked)], &records);
        let mut transactions = TransactionUpdateTracker::new(records);

        let account = accounts.accounts.get_mut(&local.id()).unwrap();
        account.proven_state = Some(winner.to_commitment());
        account.state =
            account.reconcile_public_account(&winner).with_update(patch_update(&winner));
        let pending = transactions.pending_by_account().next().unwrap();
        accounts.reconcile_pending(pending, &DiscardConditions::default());

        let discarded: BTreeMap<_, _> = transactions
            .discarded_transactions()
            .map(|tx| (tx.id, tx.status.clone()))
            .collect();
        assert_eq!(discarded, expected);
    }

    /// The reconciliation decides whether an authenticated public state replaces the local state
    /// and whether it supersedes the local state.
    #[test]
    fn public_account_update_follows_the_reconciliation() {
        let local = header(public_account_id(), 2, 2);
        // The network state, whether it replaces the local state, and whether it supersedes it.
        let cases = [
            (header(public_account_id(), 1, 1), false, false),
            (local.clone(), false, false),
            (header(public_account_id(), 3, 3), true, false),
            (header(public_account_id(), 2, 9), true, true),
        ];

        for (network, replaces, supersedes) in cases {
            let mut tracker =
                AccountUpdateTracker::new([(local.clone(), AccountStatus::Tracked)], &[]);
            let state = tracker.accounts[&local.id()]
                .reconcile_public_account(&network)
                .with_update(patch_update(&network));
            assert_eq!(matches!(state, AccountSyncState::Superseded(_)), supersedes);
            tracker.accounts.get_mut(&local.id()).unwrap().state = state;

            let updates = tracker.into_account_updates();
            let nonces: Vec<Felt> = updates
                .updated_public_accounts()
                .iter()
                .map(PublicAccountUpdate::nonce)
                .collect();
            let expected = if replaces { vec![network.nonce()] } else { vec![] };
            assert_eq!(nonces, expected, "network state {network:?}");
        }
    }
}

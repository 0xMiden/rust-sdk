use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::cmp::Ordering;

use miden_protocol::Word;
use miden_protocol::account::{AccountHeader, AccountId};
use miden_protocol::block::BlockNumber;

use super::{AccountUpdates, PublicAccountUpdate};
use crate::ClientError;
use crate::transaction::{TransactionRecord, TransactionStatus};

/// Reconciles proven account states with the state at the start of a sync.
///
/// Pending transaction links explain local states that are ahead of the chain. The transaction
/// tracker owns transaction statuses. This tracker only reports superseded account commitments.
pub(crate) struct AccountUpdateTracker {
    block_from: BlockNumber,
    accounts: BTreeMap<AccountId, AccountHeader>,
    pending_parents: BTreeMap<(AccountId, Word), Word>,
    updates: BTreeMap<AccountId, PublicAccountUpdate>,
    mismatches: BTreeMap<AccountId, Word>,
    superseded: BTreeSet<Word>,
}

pub(crate) enum AccountReconciliation {
    Keep,
    Apply,
    Supersede(Word),
}

impl AccountUpdateTracker {
    pub(crate) fn new(
        block_from: BlockNumber,
        accounts: &[AccountHeader],
        transactions: &[TransactionRecord],
    ) -> Self {
        Self {
            block_from,
            accounts: accounts.iter().map(|header| (header.id(), header.clone())).collect(),
            pending_parents: transactions
                .iter()
                .filter(|tx| matches!(tx.status, TransactionStatus::Pending))
                .map(|tx| {
                    (
                        (tx.details.account_id, tx.details.final_account_state),
                        tx.details.init_account_state,
                    )
                })
                .collect(),
            updates: BTreeMap::new(),
            mismatches: BTreeMap::new(),
            superseded: BTreeSet::new(),
        }
    }

    /// Classifies a header after its proof has been verified against the sync target.
    pub(crate) fn reconcile(
        &self,
        header: &AccountHeader,
    ) -> Result<AccountReconciliation, ClientError> {
        let local = self.accounts.get(&header.id()).ok_or_else(|| {
            ClientError::ChainValidationError(format!(
                "account {} was not seeded for sync",
                header.id()
            ))
        })?;
        let local_commitment = local.to_commitment();
        let commitment = header.to_commitment();
        if commitment == local_commitment {
            return Ok(AccountReconciliation::Keep);
        }
        match header.nonce().as_canonical_u64().cmp(&local.nonce().as_canonical_u64()) {
            Ordering::Greater => Ok(AccountReconciliation::Apply),
            Ordering::Equal
                if self.pending_parents.contains_key(&(header.id(), local_commitment)) =>
            {
                Ok(AccountReconciliation::Supersede(local_commitment))
            },
            Ordering::Less
                if self.is_pending_ancestor(header.id(), local_commitment, commitment) =>
            {
                Ok(AccountReconciliation::Keep)
            },
            _ => Err(ClientError::ChainValidationError(format!(
                "account {} at sync target conflicts with local state without a matching pending transaction chain",
                header.id()
            ))),
        }
    }

    fn is_pending_ancestor(&self, id: AccountId, mut current: Word, ancestor: Word) -> bool {
        let mut visited = BTreeSet::new();
        while visited.insert(current) {
            let Some(parent) = self.pending_parents.get(&(id, current)) else {
                return false;
            };
            if *parent == ancestor {
                return true;
            }
            current = *parent;
        }
        false
    }

    pub(crate) fn record_public(
        &mut self,
        update: Option<PublicAccountUpdate>,
        superseded: Option<Word>,
    ) {
        if let Some(commitment) = superseded {
            self.superseded.insert(commitment);
        }
        if let Some(update) = update {
            self.updates.insert(update.id(), update);
        }
    }

    pub(crate) fn record_private(&mut self, id: AccountId, commitment: Word) {
        self.mismatches.insert(id, commitment);
    }

    pub(crate) fn finish(self) -> (AccountUpdates, Vec<Word>) {
        let updates = AccountUpdates::new(
            self.updates.into_values().collect(),
            self.mismatches.into_iter().collect(),
        )
        .with_base(self.block_from, self.accounts.into_values().collect());
        (updates, self.superseded.into_iter().collect())
    }
}

/// Verifies the account, block, and account root before the caller uses the state.
pub(crate) fn verify_account_proof(
    proof: crate::rpc::domain::account::AccountProof,
    proof_block_num: BlockNumber,
    account_id: AccountId,
    chain_tip_header: &miden_protocol::block::BlockHeader,
) -> Result<(Word, Option<crate::rpc::domain::account::AccountDetails>), ClientError> {
    let target_block_num = chain_tip_header.block_num();

    if proof_block_num != target_block_num {
        return Err(ClientError::ChainValidationError(format!(
            "get_account returned block {proof_block_num} but {target_block_num} was requested"
        )));
    }

    let (witness, details) = proof.into_parts();

    // The witness is internally consistent but not yet tied to the account we requested.
    if witness.id() != account_id {
        return Err(ClientError::ChainValidationError(format!(
            "get_account returned account {} but {account_id} was requested",
            witness.id()
        )));
    }

    let account_key = miden_protocol::block::account_tree::AccountIdKey::from(account_id).as_word();
    let state_commitment = witness.state_commitment();
    witness
        .into_proof()
        .verify_presence(&account_key, &state_commitment, &chain_tip_header.account_root())
        .map_err(|err| {
            ClientError::ChainValidationError(format!(
                "get_account witness for account {account_id} does not open under block \
                     {target_block_num} account root: {err}"
            ))
        })?;

    Ok((state_commitment, details))
}

#[cfg(test)]
pub(super) mod tests {
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;
    use miden_protocol::transaction::{RawOutputNotes, TransactionId};
    use miden_protocol::{EMPTY_WORD, Felt};

    use super::*;
    use crate::transaction::TransactionDetails;

    pub(crate) fn pending(id: AccountId, initial: Word, final_state: Word) -> TransactionRecord {
        TransactionRecord::new(
            TransactionId::from_raw(final_state),
            TransactionDetails {
                account_id: id,
                init_account_state: initial,
                final_account_state: final_state,
                input_note_nullifiers: Vec::new(),
                output_notes: RawOutputNotes::new(Vec::new()).unwrap(),
                block_num: BlockNumber::GENESIS,
                submission_height: BlockNumber::GENESIS,
                expiration_block_num: BlockNumber::from(1000),
                creation_timestamp: 0,
            },
            None,
            TransactionStatus::Pending,
        )
    }

    fn header(nonce: u32, root: u32) -> AccountHeader {
        AccountHeader::new(
            ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE.try_into().unwrap(),
            Felt::from(nonce),
            [Felt::from(root); 4].into(),
            EMPTY_WORD,
            EMPTY_WORD,
        )
    }

    #[test]
    fn newer_local_state_requires_pending_ancestry() {
        let confirmed = header(1, 1);
        let intermediate = header(2, 2);
        let local = header(3, 3);
        let transactions = [
            pending(local.id(), confirmed.to_commitment(), intermediate.to_commitment()),
            pending(local.id(), intermediate.to_commitment(), local.to_commitment()),
        ];
        let tracker = AccountUpdateTracker::new(
            BlockNumber::from(100),
            core::slice::from_ref(&local),
            &transactions,
        );
        assert!(matches!(tracker.reconcile(&confirmed), Ok(AccountReconciliation::Keep)));
        assert!(matches!(tracker.reconcile(&intermediate), Ok(AccountReconciliation::Keep)));
        assert!(tracker.reconcile(&header(2, 9)).is_err());
        let unproven = AccountUpdateTracker::new(BlockNumber::from(100), &[local], &[]);
        assert!(unproven.reconcile(&intermediate).is_err());
    }

    #[test]
    fn repeated_state_is_kept_and_pending_conflict_is_superseded() {
        let local = header(2, 2);
        let tx = pending(local.id(), header(1, 1).to_commitment(), local.to_commitment());
        let tracker =
            AccountUpdateTracker::new(BlockNumber::GENESIS, core::slice::from_ref(&local), &[tx]);
        assert!(matches!(tracker.reconcile(&local), Ok(AccountReconciliation::Keep)));
        assert!(
            matches!(tracker.reconcile(&header(2, 9)), Ok(AccountReconciliation::Supersede(commitment)) if commitment == local.to_commitment())
        );
        assert!(matches!(tracker.reconcile(&header(3, 9)), Ok(AccountReconciliation::Apply)));
    }
}

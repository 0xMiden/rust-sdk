use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::account_tree::AccountWitness;

use super::{AccountUpdates, PublicAccountUpdate};

/// Collects account changes and validated witnesses during one sync.
#[derive(Default)]
pub(super) struct AccountUpdateTracker {
    public_updates: Vec<PublicAccountUpdate>,
    private_mismatches: Vec<(AccountId, Word)>,
    witnesses: BTreeMap<AccountId, AccountWitness>,
}

impl AccountUpdateTracker {
    pub(super) fn apply_public_account_update(&mut self, update: PublicAccountUpdate) {
        self.public_updates.push(update);
    }

    pub(super) fn apply_private_account_commitment(&mut self, id: AccountId, commitment: Word) {
        self.private_mismatches.push((id, commitment));
    }

    pub(super) fn has_account_witness(&self, id: AccountId) -> bool {
        self.witnesses.contains_key(&id)
    }

    pub(super) fn insert_account_witness(&mut self, id: AccountId, witness: AccountWitness) {
        self.witnesses.insert(id, witness);
    }

    pub(super) fn into_account_updates(self) -> AccountUpdates {
        AccountUpdates::new(self.public_updates, self.private_mismatches)
            .with_account_witnesses(self.witnesses.into_iter().collect())
    }
}

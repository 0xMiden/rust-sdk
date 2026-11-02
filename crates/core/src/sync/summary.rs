use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::cmp::max;

use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::NoteId;
use miden_protocol::transaction::TransactionId;

use super::{PublicAccountUpdate, StateSyncUpdate};
use crate::store::NoteUpdateType;

// SYNC SUMMARY
// ================================================================================================

/// Contains stats about the sync operation.
#[derive(Debug, PartialEq)]
pub struct SyncSummary {
    /// Block number up to which the client has been synced.
    pub block_num: BlockNumber,
    /// IDs of new public notes that the client has received.
    pub new_public_notes: Vec<NoteId>,
    /// IDs of private notes imported from the Note Transport Layer in this sync. They are still
    /// `Expected` until observed on-chain.
    ///
    /// Only populated by `Client::sync_state`. `Client::sync_chain` always leaves this empty
    /// because it does not touch the Note Transport Layer.
    pub new_private_notes: Vec<NoteId>,
    /// IDs of tracked notes that have been committed.
    pub committed_notes: Vec<NoteId>,
    /// IDs of notes that have been consumed.
    pub consumed_notes: Vec<NoteId>,
    /// IDs of on-chain accounts that have been updated.
    pub updated_accounts: Vec<AccountId>,
    /// IDs of private accounts that have been locked.
    pub locked_accounts: Vec<AccountId>,
    /// IDs of committed transactions.
    pub committed_transactions: Vec<TransactionId>,
}

impl SyncSummary {
    pub fn new(
        block_num: BlockNumber,
        new_public_notes: Vec<NoteId>,
        new_private_notes: Vec<NoteId>,
        committed_notes: Vec<NoteId>,
        consumed_notes: Vec<NoteId>,
        updated_accounts: Vec<AccountId>,
        locked_accounts: Vec<AccountId>,
        committed_transactions: Vec<TransactionId>,
    ) -> Self {
        Self {
            block_num,
            new_public_notes,
            new_private_notes,
            committed_notes,
            consumed_notes,
            updated_accounts,
            locked_accounts,
            committed_transactions,
        }
    }

    pub fn new_empty(block_num: BlockNumber) -> Self {
        Self {
            block_num,
            new_public_notes: vec![],
            new_private_notes: vec![],
            committed_notes: vec![],
            consumed_notes: vec![],
            updated_accounts: vec![],
            locked_accounts: vec![],
            committed_transactions: vec![],
        }
    }

    pub fn is_empty(&self) -> bool {
        self.new_public_notes.is_empty()
            && self.new_private_notes.is_empty()
            && self.committed_notes.is_empty()
            && self.consumed_notes.is_empty()
            && self.updated_accounts.is_empty()
            && self.locked_accounts.is_empty()
            && self.committed_transactions.is_empty()
    }

    pub fn combine_with(&mut self, mut other: Self) {
        self.block_num = max(self.block_num, other.block_num);
        self.new_public_notes.append(&mut other.new_public_notes);
        self.new_private_notes.append(&mut other.new_private_notes);
        self.committed_notes.append(&mut other.committed_notes);
        self.consumed_notes.append(&mut other.consumed_notes);
        self.updated_accounts.append(&mut other.updated_accounts);
        self.locked_accounts.append(&mut other.locked_accounts);
        self.committed_transactions.append(&mut other.committed_transactions);
    }
}

impl From<&StateSyncUpdate> for SyncSummary {
    fn from(value: &StateSyncUpdate) -> Self {
        let new_public_note_ids = value
            .note_updates()
            .input_notes()
            .iter()
            .filter_map(|note_update| {
                let note = note_update.inner();
                if let NoteUpdateType::Insert = note_update.update_type() {
                    note.id()
                } else {
                    None
                }
            })
            .collect();

        let committed_note_ids: BTreeSet<NoteId> = value
            .note_updates()
            .input_notes()
            .iter()
            .filter_map(|note_update| {
                let note = note_update.inner();
                // `InsertCommitted` is a previously-tracked expected note that just committed, so
                // it counts as committed (not as a newly-discovered note) even though it is
                // persisted via a full-row insert.
                if matches!(
                    note_update.update_type(),
                    NoteUpdateType::Update | NoteUpdateType::InsertCommitted
                ) && note.is_committed()
                {
                    note.id()
                } else {
                    None
                }
            })
            .chain(value.note_updates().output_notes().iter().filter_map(|note_update| {
                let note = note_update.inner();
                if let NoteUpdateType::Update = note_update.update_type() {
                    note.is_committed().then_some(note.id())
                } else {
                    None
                }
            }))
            .collect();

        let consumed_note_ids: BTreeSet<NoteId> =
            value.note_updates().consumed_input_note_ids().collect();

        SyncSummary::new(
            value.block_num(),
            new_public_note_ids,
            // Populated by Client::sync_state from the Note Transport Layer fetch.
            Vec::new(),
            committed_note_ids.into_iter().collect(),
            consumed_note_ids.into_iter().collect(),
            value
                .account_updates()
                .updated_public_accounts()
                .iter()
                .map(PublicAccountUpdate::id)
                .collect(),
            value
                .account_updates()
                .mismatched_private_accounts()
                .iter()
                .map(|(id, _)| *id)
                .collect(),
            value.transaction_updates().committed_transactions().map(|t| t.id).collect(),
        )
    }
}

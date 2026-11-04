use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{AccountId, StorageSlotName};
use miden_protocol::crypto::merkle::mmr::{Forest, InOrderIndex};
use miden_protocol::note::{NoteDetailsCommitment, NoteId, NoteScriptRoot, Nullifier};
use miden_protocol::transaction::TransactionId;

use crate::transaction::TransactionStatusVariant;

// PARTIAL BLOCKCHAIN NODE FILTER
// ================================================================================================

/// Filters for searching specific MMR nodes.
// TODO: Should there be filters for specific blocks instead of nodes?
pub enum PartialBlockchainFilter {
    /// Return all nodes.
    All,
    /// Filter by the specified in-order indices.
    List(Vec<InOrderIndex>),
    /// Return nodes with in-order indices within the specified forest.
    Forest(Forest),
}

// NOTE FILTER
// ================================================================================================

/// Filters for narrowing the set of notes returned by the client's store.
#[derive(Debug, Clone)]
pub enum NoteFilter {
    /// Return a list of all notes ([`InputNoteRecord`](crate::store::InputNoteRecord) or
    /// [`OutputNoteRecord`](crate::store::OutputNoteRecord)).
    All,
    /// Return a list of committed notes ([`InputNoteRecord`](crate::store::InputNoteRecord) or
    /// [`OutputNoteRecord`](crate::store::OutputNoteRecord)). These represent notes that the
    /// blockchain has included in a block.
    Committed,
    /// Filter by consumed notes ([`InputNoteRecord`](crate::store::InputNoteRecord) or
    /// [`OutputNoteRecord`](crate::store::OutputNoteRecord)). notes that have been used as inputs
    /// in transactions.
    Consumed,
    /// Return a list of expected notes ([`InputNoteRecord`](crate::store::InputNoteRecord) or
    /// [`OutputNoteRecord`](crate::store::OutputNoteRecord)). These represent notes for which the
    /// store doesn't have anchor data.
    Expected,
    /// Return a list containing any notes that match with the provided [`NoteId`] vector.
    List(Vec<NoteId>),
    /// Return a list containing any notes whose details commitment matches one of the provided
    /// [`NoteDetailsCommitment`] vector. Unlike [`NoteFilter::List`], this matches the
    /// metadata-independent details commitment, so it also resolves metadata-less notes (which have
    /// a NULL `note_id`).
    DetailsCommitments(Vec<NoteDetailsCommitment>),
    /// Return a list containing any notes that match the provided [`Nullifier`] vector.
    Nullifiers(Vec<Nullifier>),
    /// Return a list of notes that are currently being processed. This filter doesn't apply to
    /// output notes.
    Processing,
    /// Return a list containing any notes whose script root matches one of the provided
    /// [`NoteScriptRoot`]s. Notes whose script isn't known (e.g. partial output notes) never match.
    ScriptRoots(Vec<NoteScriptRoot>),
    /// Return a list containing the note that matches with the provided [`NoteId`]. The query will
    /// return an error if the note isn't found.
    Unique(NoteId),
    /// Return a list containing notes that haven't been nullified yet, this includes expected,
    /// committed, processing and unverified notes.
    Unspent,
    /// Return a list containing notes with unverified inclusion proofs. This filter doesn't apply
    /// to output notes.
    Unverified,
}

// TRANSACTION FILTERS
// ================================================================================================

/// Filters for narrowing the set of transactions returned by the client's store.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum TransactionFilter {
    /// Return all transactions.
    All,
    /// Filter by transactions that haven't yet been committed to the blockchain as per the last
    /// sync.
    Uncommitted,
    /// Return a list of the transaction that matches the provided [`TransactionId`]s.
    Ids(Vec<TransactionId>),
    /// Return the transactions that match every criterion of the query, newest first.
    Query(TransactionFilterQuery),
}

/// The criteria of [`TransactionFilter::Query`]. A criterion that is `None` matches every
/// transaction.
///
/// Transactions are ordered by creation time, newest first. Transactions with the same creation
/// time are ordered by descending ID.
#[derive(Debug, Clone, Default)]
pub struct TransactionFilterQuery {
    /// Keep only the transactions executed by this account.
    pub account_id: Option<AccountId>,
    /// Keep only the transactions in this status.
    pub status: Option<TransactionStatusVariant>,
    /// Keep only the newest transactions, at most this many.
    pub limit: Option<u32>,
}

// STORAGE FILTER
// ================================================================================================

/// Filters for narrowing the storage slots returned by the client's store.
#[derive(Debug, Clone)]
pub enum AccountStorageFilter {
    /// Return an [`AccountStorage`](miden_protocol::account::AccountStorage) with all available
    /// slots.
    All,
    /// Return an [`AccountStorage`](miden_protocol::account::AccountStorage) with a single slot
    /// that matches the provided [`Word`] map root.
    Root(Word),
    /// Return an [`AccountStorage`](miden_protocol::account::AccountStorage) with a single slot
    /// that matches the provided slot name.
    SlotName(StorageSlotName),
    /// Return an [`AccountStorage`](miden_protocol::account::AccountStorage) containing only the
    /// slots whose names are in the provided list. Useful to avoid loading the full storage when
    /// only a known subset of slots is needed (e.g. when applying a delta to a large account).
    SlotNames(Vec<StorageSlotName>),
}

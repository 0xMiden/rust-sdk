//! Defines the storage interfaces used by the Miden client.
//!
//! It provides mechanisms for persisting and retrieving data, such as account states, transaction
//! history, block headers, notes, and MMR nodes.
//!
//! ## Overview
//!
//! The storage module is central to the Miden client’s persistence layer. It defines the [`Store`]
//! trait which abstracts over any concrete storage implementation. The trait exposes methods to
//! (among others):
//!
//! - Retrieve and update transactions, notes, and accounts.
//! - Store and query block headers along with MMR peaks and authentication nodes.
//! - Manage note tags for synchronizing with the node.
//!
//! These are all used by the Miden client to provide transaction execution in the correct contexts.
//!
//! In addition to the main [`Store`] trait, the module provides types for filtering queries, such
//! as [`TransactionFilter`], [`NoteFilter`], `StorageFilter` to narrow down the set of returned
//! transactions, account data, or notes. For more advanced usage, see the documentation of
//! individual methods in the [`Store`] trait.

/// Contains [`ClientDataStore`] to automatically implement [`DataStore`] for anything that
/// implements [`Store`]. This isn't public because it's an implementation detail to instantiate the
/// executor.
///
/// The user is tasked with creating a [`Store`] which the client will wrap into a
/// [`ClientDataStore`] at creation time.
pub(crate) mod data_store;

pub use miden_client_core::store::{
    AccountRecord,
    AccountRecordData,
    AccountRecordError,
    AccountSmtForest,
    AccountStatus,
    AccountStorageFilter,
    AccountUpdate,
    BlockRelevance,
    ClientAccountType,
    InputNoteCursor,
    InputNoteRecord,
    InputNoteState,
    NoteExportType,
    NoteFilter,
    NoteRecordError,
    OutputNoteRecord,
    OutputNoteState,
    PartialBlockchainFilter,
    SettingMutation,
    SettingScope,
    Store,
    StoreError,
    TransactionFilter,
    TransactionFilterQuery,
    input_note_states,
};

pub use crate::sync::PublicAccountUpdate;

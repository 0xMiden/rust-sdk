//! Re-exports the storage types from `miden-client-core` that the client API uses, and adapts a
//! [`Store`] to the data store of the transaction executor.

/// Contains [`ClientDataStore`] to automatically implement [`DataStore`] for anything that
/// implements [`Store`]. This isn't public because it's an implementation detail to instantiate the
/// executor.
///
/// The user is tasked with creating a [`Store`] which the client will wrap into a
/// [`ClientDataStore`] at creation time.
pub(crate) mod data_store;

pub use miden_client_core::errors::{NoteRecordError, StoreError};
pub use miden_client_core::store::{
    AccountStatus,
    BlockRelevance,
    InputNoteRecord,
    InputNoteState,
    NoteExportType,
    NoteFilter,
    OutputNoteRecord,
    OutputNoteState,
    Store,
    TransactionFilter,
    TransactionFilterQuery,
    input_note_states,
};

pub use crate::sync::PublicAccountUpdate;

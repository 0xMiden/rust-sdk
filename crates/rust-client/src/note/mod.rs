//! Contains the Client APIs related to notes. Notes can contain assets and scripts that are
//! executed as part of transactions.
//!
//! This module enables the tracking, retrieval, and processing of notes.
//! It offers methods to query input and output notes from the store, check their consumability,
//! compile note scripts, and retrieve notes based on partial ID matching.
//!
//! ## Overview
//!
//! The module exposes APIs to:
//!
//! - Retrieve input notes and output notes.
//! - Determine the consumability of notes using the [`NoteScreener`].
//! - Compile note scripts from source code with `compile_note_script`.
//! - Retrieve an input note by a prefix of its ID using the helper function
//!   [`get_input_note_with_id_prefix`].
//!
//! ## Example
//!
//! ```rust
//! use miden_client::{
//!     auth::TransactionAuthenticator,
//!     Client,
//!     crypto::FeltRng,
//!     note::{NoteScreener, get_input_note_with_id_prefix},
//!     store::NoteFilter,
//! };
//! use miden_protocol::account::AccountId;
//!
//! # async fn example<AUTH: TransactionAuthenticator + Sync>(client: &Client<AUTH>) -> Result<(), Box<dyn std::error::Error>> {
//! // Retrieve all committed input notes
//! let input_notes = client.get_input_notes(NoteFilter::Committed).await?;
//! println!("Found {} committed input notes.", input_notes.len());
//!
//! // Check consumability for a specific note
//! if let Some(note) = input_notes.first() {
//!     let consumability = client.get_note_consumability(note.clone()).await?;
//!     println!("Note consumability: {:?}", consumability);
//! }
//!
//! // Retrieve an input note by a partial ID match
//! let note_prefix = "0x70b7ec";
//! match get_input_note_with_id_prefix(client, note_prefix).await {
//!     Ok(note) => println!(
//!         "Found note with matching prefix: {}",
//!         note.id().expect("note matched by ID prefix has an ID").to_hex()
//!     ),
//!     Err(err) => println!("Error retrieving note: {err:?}"),
//! }
//!
//! // Compile the note script
//! let script_src = "@note_script\npub proc main\n    push.9 push.12 add\nend";
//! let note_script = client.code_builder().compile_note_script(script_src)?;
//! println!("Compiled note script successfully.");
//!
//! # Ok(())
//! # }
//! ```
//!
//! For more details on the API and error handling, see the documentation for the specific functions
//! and types in this module.

use alloc::vec::Vec;

use miden_protocol::account::AccountId;
use miden_tx::auth::TransactionAuthenticator;

use crate::store::{InputNoteRecord, NoteFilter, OutputNoteRecord};
use crate::{Client, ClientError, IdPrefixFetchError};

mod import;
mod note_reader;
mod note_screener;
mod note_update_tracker;

// RE-EXPORTS
// ================================================================================================

pub use miden_protocol::block::BlockNumber;
pub use miden_protocol::errors::NoteError;
pub use miden_protocol::note::{
    Note,
    NoteAssets,
    NoteAttachment,
    NoteAttachmentContent,
    NoteAttachmentHeader,
    NoteAttachmentScheme,
    NoteAttachments,
    NoteDetails,
    NoteDetailsCommitment,
    NoteHeader,
    NoteId,
    NoteInclusionProof,
    NoteLocation,
    NoteMetadata,
    NoteRecipient,
    NoteScript,
    NoteScriptRoot,
    NoteStorage,
    NoteTag,
    NoteType,
    Nullifier,
    PartialNote,
    PartialNoteMetadata,
};
pub use miden_protocol::transaction::ToInputNoteCommitments;
/// Raw access to `miden-standards` note modules for items not curated by `miden-client`.
pub use miden_standards::note as standards;
pub use miden_standards::note::{
    MintNote,
    MintNoteStorage,
    NetworkAccountTarget,
    NoteConsumptionStatus,
    NoteExecutionHint,
    NoteFile,
    NoteSyncHint,
    P2idNote,
    P2idNoteStorage,
    P2ideNote,
    P2ideNoteStorage,
    PswapNote,
    StandardNote,
    SwapNote,
};
pub use miden_tx::{FailedNote, NoteConsumptionInfo};
pub use note_reader::InputNoteReader;
pub use note_screener::{NoteConsumability, NoteScreener, NoteScreenerError};
pub use note_update_tracker::{
    InputNoteUpdate,
    NoteConsumption,
    NoteUpdateTracker,
    NoteUpdateType,
    OutputNoteUpdate,
};

/// Note retrieval methods.
impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync,
{
    // INPUT NOTE DATA RETRIEVAL
    // --------------------------------------------------------------------------------------------

    /// Retrieves the input notes managed by the client from the store.
    ///
    /// # Errors
    ///
    /// Returns a [`ClientError::StoreError`] if the filter is [`NoteFilter::Unique`] and there is
    /// no Note with the provided ID.
    pub async fn get_input_notes(
        &self,
        filter: NoteFilter,
    ) -> Result<Vec<InputNoteRecord>, ClientError> {
        self.store.get_input_notes(filter).await.map_err(Into::into)
    }

    /// Returns the input notes and their consumability. Assuming the notes will be consumed by a
    /// normal consume transaction. If `account_id` is None then all consumable input notes are
    /// returned.
    ///
    /// The note screener runs a series of checks to determine whether the note can be executed as
    /// part of a transaction for a specific account. If the specific account ID can consume it (ie,
    /// if it's compatible with the account), it will be returned as part of the result list.
    ///
    /// # Performance
    ///
    /// This call screens every committed note tracked by the client on each invocation, without
    /// retaining verdicts between calls. When `account_id` is `None` the notes are screened against
    /// every account tracked by the client; when it is `Some`, only against that account. For notes
    /// whose consumability cannot be determined statically, the screener runs one trial transaction
    /// in the VM per `(account, note)` pair, so the cost grows with the number of screened accounts
    /// multiplied by the number of committed notes.
    ///
    /// Consider cheaper alternatives when calling this function for accounts that accumulate
    /// committed-unconsumed notes, especially when used in polling loops:
    ///
    /// - Query and filter the notes directly with [`Self::get_input_notes`] and
    ///   [`NoteFilter::Committed`] if note consumability verdict is not needed.
    /// - Wait for a specific note to commit with [`Self::get_input_note`] and
    ///   [`InputNoteRecord::is_committed`], instead of polling for it in the screened results.
    /// - Screen a narrower set of notes with [`NoteScreener::can_consume_batch`] or
    ///   [`NoteScreener::can_consume_batch_for_account`], reached through [`Self::note_screener`].
    pub async fn get_consumable_notes(
        &self,
        account_id: Option<AccountId>,
    ) -> Result<Vec<(InputNoteRecord, Vec<NoteConsumability>)>, ClientError> {
        let committed_notes = self.store.get_input_notes(NoteFilter::Committed).await?;
        let notes = committed_notes
            .iter()
            .cloned()
            .map(TryInto::try_into)
            .collect::<Result<Vec<Note>, _>>()?;

        let note_screener = self.note_screener();
        let mut note_relevances = match account_id {
            Some(account_id) => {
                note_screener.can_consume_batch_for_account(account_id, &notes).await?
            },
            None => note_screener.can_consume_batch(&notes).await?,
        };

        let mut relevant_notes = Vec::new();
        for input_note in committed_notes {
            // Committed notes always have metadata, so id() is `Some`.
            let Some(note_id) = input_note.id() else { continue };
            // A note is in the map only when at least one screened account can consume it, so its
            // relevance list is never empty.
            let Some(account_relevance) = note_relevances.remove(&note_id) else {
                continue;
            };

            relevant_notes.push((input_note, account_relevance));
        }

        Ok(relevant_notes)
    }

    /// Returns the consumability conditions for the provided note.
    ///
    /// The note screener runs a series of checks to determine whether the note can be executed as
    /// part of a transaction for a specific account. If the specific account ID can consume it (ie,
    /// if it's compatible with the account), it will be returned as part of the result list.
    pub async fn get_note_consumability(
        &self,
        note: InputNoteRecord,
    ) -> Result<Vec<NoteConsumability>, ClientError> {
        self.note_screener().can_consume(&note.try_into()?).await.map_err(Into::into)
    }

    /// Retrieves the input note given a [`NoteId`]. Returns `None` if the note is not found.
    pub async fn get_input_note(
        &self,
        note_id: NoteId,
    ) -> Result<Option<InputNoteRecord>, ClientError> {
        Ok(self.store.get_input_notes(NoteFilter::Unique(note_id)).await?.pop())
    }

    // OUTPUT NOTE DATA RETRIEVAL
    // --------------------------------------------------------------------------------------------

    /// Returns output notes managed by this client.
    pub async fn get_output_notes(
        &self,
        filter: NoteFilter,
    ) -> Result<Vec<OutputNoteRecord>, ClientError> {
        self.store.get_output_notes(filter).await.map_err(Into::into)
    }

    /// Retrieves the output note given a [`NoteId`]. Returns `None` if the note is not found.
    pub async fn get_output_note(
        &self,
        note_id: NoteId,
    ) -> Result<Option<OutputNoteRecord>, ClientError> {
        Ok(self.store.get_output_notes(NoteFilter::Unique(note_id)).await?.pop())
    }

    /// Returns an [`InputNoteReader`] that lazily iterates over consumed input notes
    /// for the given consumer account.
    ///
    /// The consumer is required because ordering is only guaranteed among notes
    /// consumed by the same account.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let mut reader = client.input_note_reader(account_id);
    ///
    /// while let Some(note) = reader.next().await? {
    ///     process(note);
    /// }
    /// ```
    pub fn input_note_reader(&self, consumer: AccountId) -> InputNoteReader {
        InputNoteReader::new(self.store.clone(), consumer)
    }
}

/// Returns the client input note whose ID starts with `note_id_prefix`.
///
/// # Errors
///
/// - Returns [`IdPrefixFetchError::NoMatch`] if we were unable to find any note where
///   `note_id_prefix` is a prefix of its ID.
/// - Returns [`IdPrefixFetchError::MultipleMatches`] if there were more than one note found where
///   `note_id_prefix` is a prefix of its ID.
pub async fn get_input_note_with_id_prefix<AUTH>(
    client: &Client<AUTH>,
    note_id_prefix: &str,
) -> Result<InputNoteRecord, IdPrefixFetchError>
where
    AUTH: TransactionAuthenticator + Sync,
{
    let mut input_note_records = client
        .get_input_notes(NoteFilter::All)
        .await
        .map_err(|err| {
            tracing::error!("Error when fetching all notes from the store: {err}");
            IdPrefixFetchError::NoMatch(format!("note ID prefix {note_id_prefix}"))
        })?
        .into_iter()
        .filter(|note_record| {
            note_record.id().is_some_and(|id| id.to_hex().starts_with(note_id_prefix))
        })
        .collect::<Vec<_>>();

    if input_note_records.is_empty() {
        return Err(IdPrefixFetchError::NoMatch(format!("note ID prefix {note_id_prefix}")));
    }
    if input_note_records.len() > 1 {
        let input_note_record_ids =
            input_note_records.iter().map(InputNoteRecord::id).collect::<Vec<_>>();
        tracing::error!(
            "Multiple notes found for the prefix {}: {:?}",
            note_id_prefix,
            input_note_record_ids
        );
        return Err(IdPrefixFetchError::MultipleMatches(format!(
            "note ID prefix {note_id_prefix}"
        )));
    }

    Ok(input_note_records
        .pop()
        .expect("input_note_records should always have one element"))
}

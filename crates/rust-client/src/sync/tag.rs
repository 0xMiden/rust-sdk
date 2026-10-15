use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{NoteDetailsCommitment, NoteTag};

use crate::Client;
use crate::errors::ClientError;
use crate::store::{InputNoteRecord, NoteRecordError};

/// Tag management methods
impl<AUTH> Client<AUTH> {
    /// Returns the list of note tags tracked by the client along with their source.
    ///
    /// When syncing the state with the node, these tags will be added to the sync request and
    /// note-related information will be retrieved for notes that have matching tags.
    ///  The source of the tag indicates its origin. It helps distinguish between:
    ///  - Tags added manually by the user.
    ///  - Tags automatically added by the client to track notes.
    ///  - Tags added for accounts tracked by the client.
    ///
    /// Note: Tags for accounts that are being tracked by the client are managed automatically by
    /// the client and don't need to be added here. That is, notes for managed accounts will be
    /// retrieved automatically by the client when syncing.
    pub async fn get_note_tags(&self) -> Result<Vec<NoteTagRecord>, ClientError> {
        let mut tags = self.store.get_note_tags().await?;
        tags.extend(self.store.get_account_note_tags().await?);
        Ok(tags)
    }

    /// Adds a note tag for the client to track. This tag's source will be marked as `User`.
    ///
    /// Returns true if the tag was added, and false if it was already being tracked.
    pub async fn add_note_tag(&mut self, tag: NoteTag) -> Result<bool, ClientError> {
        self.store
            .add_note_tag(NoteTagRecord { tag, source: NoteTagSource::User })
            .await
            .map_err(Into::into)
    }

    /// Removes a note tag for the client to track. Only tags added by the user can be removed.
    ///
    /// Returns true if the tag was removed, and false if it was not being tracked.
    pub async fn remove_note_tag(&mut self, tag: NoteTag) -> Result<bool, ClientError> {
        let removed = self
            .store
            .remove_note_tag(NoteTagRecord { tag, source: NoteTagSource::User })
            .await?;

        Ok(removed > 0)
    }
}

/// Represents a note tag of which the Store can keep track and retrieve.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct NoteTagRecord {
    pub tag: NoteTag,
    pub source: NoteTagSource,
}

/// Represents the source of the tag. This is used to differentiate between tags that are added by
/// the user and tags that are added automatically by the client to track notes .
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum NoteTagSource {
    /// Tag of an address of a tracked native account.
    ///
    /// The store derives these tags from the addresses of the native accounts and does not keep
    /// them. See [`Store::get_account_note_tags`](crate::store::Store::get_account_note_tags).
    Account(AccountId),
    /// Tag for tracked expected notes, identified by the note's details commitment.
    Note(NoteDetailsCommitment),
    /// Tag manually added by the user.
    User,
    /// Tag for a long-lived subscription, anchored to an opaque 4-felt key that identifies its
    /// origin (e.g. the id of the note that registered it). Distinct subscriptions may share the
    /// same [`NoteTag`]; the key keeps them as separate rows so each is tracked and removed
    /// independently.
    Subscription(Word),
}

impl NoteTagRecord {
    pub fn with_note_source(tag: NoteTag, details_commitment: NoteDetailsCommitment) -> Self {
        Self {
            tag,
            source: NoteTagSource::Note(details_commitment),
        }
    }

    pub fn with_account_source(tag: NoteTag, account_id: AccountId) -> Self {
        Self {
            tag,
            source: NoteTagSource::Account(account_id),
        }
    }
}

impl PartialEq<NoteTag> for NoteTagRecord {
    fn eq(&self, other: &NoteTag) -> bool {
        self.tag == *other
    }
}

impl From<&Account> for NoteTagRecord {
    fn from(account: &Account) -> Self {
        NoteTagRecord::with_account_source(NoteTag::with_account_target(account.id()), account.id())
    }
}

impl TryInto<NoteTagRecord> for &InputNoteRecord {
    type Error = NoteRecordError;

    fn try_into(self) -> Result<NoteTagRecord, Self::Error> {
        match self.metadata() {
            Some(metadata) => {
                Ok(NoteTagRecord::with_note_source(metadata.tag(), self.details_commitment()))
            },
            None => Err(NoteRecordError::ConversionError(
                "Input Note Record does not contain tag".to_string(),
            )),
        }
    }
}

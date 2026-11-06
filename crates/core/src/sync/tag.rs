use alloc::string::ToString;

use miden_protocol::Word;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{NoteDetailsCommitment, NoteTag};

use crate::errors::NoteRecordError;
use crate::store::InputNoteRecord;

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

impl TryFrom<&InputNoteRecord> for NoteTagRecord {
    type Error = NoteRecordError;

    fn try_from(record: &InputNoteRecord) -> Result<Self, Self::Error> {
        match record.metadata() {
            Some(metadata) => {
                Ok(NoteTagRecord::with_note_source(metadata.tag(), record.details_commitment()))
            },
            None => Err(NoteRecordError::ConversionError(
                "Input Note Record does not contain tag".to_string(),
            )),
        }
    }
}

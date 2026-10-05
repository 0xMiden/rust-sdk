use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::note::{NoteDetailsCommitment, NoteTag};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

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
        self.store.get_note_tags().await.map_err(Into::into)
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
    /// Tag for notes directed to a tracked account.
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

impl Serializable for NoteTagRecord {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.tag.write_into(target);
        self.source.write_into(target);
    }
}

impl Deserializable for NoteTagRecord {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let tag = NoteTag::read_from(source)?;
        let source = NoteTagSource::read_from(source)?;
        Ok(Self { tag, source })
    }
}

impl Serializable for NoteTagSource {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        match self {
            NoteTagSource::Account(account_id) => {
                target.write_u8(0);
                account_id.write_into(target);
            },
            NoteTagSource::Note(details_commitment) => {
                target.write_u8(1);
                details_commitment.write_into(target);
            },
            NoteTagSource::User => target.write_u8(2),
            NoteTagSource::Subscription(key) => {
                // Discriminant 3 must stay stable so rows survive deserialization.
                target.write_u8(3);
                key.write_into(target);
            },
        }
    }
}

impl Deserializable for NoteTagSource {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        match source.read_u8()? {
            0 => Ok(NoteTagSource::Account(AccountId::read_from(source)?)),
            1 => Ok(NoteTagSource::Note(NoteDetailsCommitment::read_from(source)?)),
            2 => Ok(NoteTagSource::User),
            3 => Ok(NoteTagSource::Subscription(Word::read_from(source)?)),
            val => Err(DeserializationError::InvalidValue(format!("Invalid tag source: {val}"))),
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

#[cfg(test)]
mod tests {
    use miden_protocol::{Felt, Word};
    use miden_tx::utils::serde::{Deserializable, Serializable};

    use super::NoteTagSource;

    #[test]
    fn subscription_note_tag_source_round_trips_with_stable_discriminant() {
        let key: Word =
            [Felt::from(1u32), Felt::from(2u32), Felt::from(3u32), Felt::from(4u32)].into();
        let source = NoteTagSource::Subscription(key);

        let bytes = source.to_bytes();
        // Discriminant byte must stay 3 so persisted rows keep deserializing across releases.
        assert_eq!(bytes[0], 3, "Subscription discriminant must remain 3");
        assert_eq!(NoteTagSource::read_from_bytes(&bytes).unwrap(), source);
    }
}

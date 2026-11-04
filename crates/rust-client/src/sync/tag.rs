use alloc::vec::Vec;

use miden_client_core::sync::{NoteTagRecord, NoteTagSource};
use miden_protocol::note::NoteTag;

use crate::Client;
use crate::errors::ClientError;

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

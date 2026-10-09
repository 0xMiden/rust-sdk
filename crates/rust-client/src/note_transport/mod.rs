pub mod errors;
pub mod generated;
#[cfg(feature = "tonic")]
pub mod grpc;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::address::Address;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{
    Note,
    NoteDetails,
    NoteDetailsCommitment,
    NoteHeader,
    NoteId,
    NoteInclusionProof,
    NoteTag,
};
use miden_protocol::utils::serde::Serializable;
use miden_tx::auth::TransactionAuthenticator;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    SliceReader,
};

pub use self::errors::NoteTransportError;
use crate::note::{NoteFile, NoteSyncHint};
use crate::store::{NoteFilter, SettingScope};
use crate::{Client, ClientError};

pub const NOTE_TRANSPORT_MAINNET_ENDPOINT: &str = "https://transport.mainnet.miden.io";
pub const NOTE_TRANSPORT_TESTNET_ENDPOINT: &str = "https://transport.miden.io";
pub const NOTE_TRANSPORT_DEVNET_ENDPOINT: &str = "https://transport.devnet.miden.io";
#[allow(deprecated)]
pub use miden_client_core::note_transport::NOTE_TRANSPORT_CURSOR_STORE_SETTING;
pub use miden_client_core::note_transport::NoteTransportCursor;
pub const NOTE_TRANSPORT_CURSORS_KEY: &str = "note_transport_cursors";

type NoteTransportCursors = BTreeMap<NoteTag, NoteTransportCursor>;
/// Maximum number of note tags in one transport fetch request. The service rejects larger requests.
const MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST: usize = 128;
/// Limits the pages for each request group so other groups can progress in the same sync.
const MAX_NOTE_TRANSPORT_PAGES_PER_GROUP: usize = 32;

/// Legacy settings key for note transport backfill state.
#[deprecated(since = "0.17.1", note = "note transport no longer keeps per-tag backfill state")]
pub const NOTE_TRANSPORT_COVERED_TAGS_KEY: &str = "note_transport_covered_tags";

/// Client note transport methods.
impl<AUTH> Client<AUTH> {
    /// Maximum number of note tags in one transport fetch request.
    pub const MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST: usize = MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST;

    /// Legacy name for [`Self::MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST`].
    ///
    /// This value is not a limit on the number of account tags that the client can track.
    #[deprecated(
        since = "0.17.1",
        note = "use MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST; account tags are no longer limited"
    )]
    pub const MAX_ACCOUNT_TAGS: usize = Self::MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST;

    /// Check if note transport connection is configured
    pub fn is_note_transport_enabled(&self) -> bool {
        self.note_transport_api.is_some()
    }

    /// Returns the Note Transport client
    ///
    /// Errors if the note transport is not configured.
    pub(crate) fn get_note_transport_api(
        &self,
    ) -> Result<Arc<dyn NoteTransportClient>, NoteTransportError> {
        self.note_transport_api.clone().ok_or(NoteTransportError::Disabled)
    }

    /// Send a note through the note transport network together with its inclusion proof.
    ///
    /// The note will be end-to-end encrypted (unimplemented, currently plaintext) using the
    /// provided recipient's `address` details. The recipient will be able to retrieve this note
    /// through the note's [`NoteTag`].
    ///
    /// The transport carries the proof through [`NoteTransportClient::send_note_with_proof`]. The
    /// network verifies `inclusion_proof` against its node before it stores the note and relays the
    /// exact commitment block to the recipient. The proof exists once the transaction that created
    /// the note is committed and the sender has synced past it; see
    /// [`OutputNoteRecord::inclusion_proof`](crate::store::OutputNoteRecord::inclusion_proof).
    ///
    /// **Failures.** The client does not keep the note for a later attempt, and no sync sends it
    /// again. An error means that the note did not reach the network or that the result is not
    /// known. A send is idempotent by note id: the network stores a note only once, and the
    /// recipient imports it only once. The caller can therefore send the same note again, now or
    /// later. For a note that this client created, the output note record keeps the note and its
    /// inclusion proof, so a later attempt can read both from the store.
    pub async fn send_private_note_with_proof(
        &mut self,
        note: Note,
        address: &Address,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), ClientError> {
        let api = self.get_note_transport_api()?;

        // The address is reserved for end-to-end encryption of the note details:
        // address.key().encrypt(note.details().to_bytes()).
        let _ = address;

        api.send_note_with_proof(TransportNote::from(note), inclusion_proof).await?;

        Ok(())
    }

    /// Loads the cursor for each tag used by the transport fetch.
    ///
    /// A missing or unreadable value resets all tags so the next fetch safely reads their retained
    /// history.
    async fn load_note_transport_cursors(&self) -> Result<NoteTransportCursors, ClientError> {
        let bytes = self
            .store
            .get_setting(SettingScope::Client, String::from(NOTE_TRANSPORT_CURSORS_KEY))
            .await
            .map_err(ClientError::StoreError)?;
        let Some(bytes) = bytes else {
            return Ok(BTreeMap::new());
        };

        match NoteTransportCursors::read_from_bytes(&bytes) {
            Ok(cursors) => Ok(cursors),
            Err(err) => {
                tracing::warn!(?err, "resetting unreadable note transport cursors");
                Ok(BTreeMap::new())
            },
        }
    }

    /// Saves the cursor for each tag used by the transport fetch.
    async fn save_note_transport_cursors(
        &self,
        cursors: &NoteTransportCursors,
    ) -> Result<(), ClientError> {
        let key = String::from(NOTE_TRANSPORT_CURSORS_KEY);
        if cursors.is_empty() {
            self.store.remove_setting(SettingScope::Client, key).await?;
        } else {
            self.store.set_setting(SettingScope::Client, key, cursors.to_bytes()).await?;
        }
        Ok(())
    }
}

impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Legacy per-sync cap for tag backfills.
    #[deprecated(since = "0.17.1", note = "note transport no longer performs per-tag backfills")]
    pub const MAX_BACKFILL_TAGS_PER_SYNC: usize = 64;

    /// Fetch notes for tracked note tags.
    ///
    /// The client queries the configured note transport node for all tracked tags. To list tracked
    /// tags, use [`Client::get_note_tags`]. To add a user-source tag, use [`Client::add_note_tag`].
    /// Fetched notes are stored in the client store.
    ///
    /// An internal pagination mechanism starts each request from the lowest cursor of its tags.
    /// Tags without a stored cursor start from the first retained note. The service can deliver a
    /// note again after a tag is added or removed; the import drops these duplicates.
    ///
    /// A failed request returns an error after successful pages are imported and their cursors are
    /// saved. Histories that exceed the page budget continue on the next call.
    pub async fn fetch_private_notes(&mut self) -> Result<(), ClientError> {
        self.ensure_genesis_in_place().await?;

        let mut update = self.fetch_transport_notes_in_chunks().await?;
        let fetch_error = update.fetch_error.take();
        self.apply_note_transport_update(update).await?;
        if let Some(error) = fetch_error {
            return Err(error);
        }

        Ok(())
    }

    /// Screens the transport-delivered notes carrying a tag derived from a tracked account,
    /// discarding those that no tracked account can consume. Notes carrying any other tag are kept
    /// as delivered.
    async fn screen_transport_notes(
        &self,
        notes: &mut Vec<(NoteId, Note, Option<BlockNumber>)>,
    ) -> Result<(), ClientError> {
        let account_tags = self.tracked_account_tags().await?;

        let notes_to_screen: Vec<Note> = notes
            .iter()
            .filter(|(_, note, _)| account_tags.contains(&note.metadata().tag()))
            .map(|(_, note, _)| note.clone())
            .collect();
        let consumable = self.note_screener().get_batch_consumability(&notes_to_screen).await?;

        // Discard the notes whose tag match the tracked accounts but are not consumable.
        notes.retain(|(_, note, _)| {
            !account_tags.contains(&note.metadata().tag()) || consumable.contains_key(&note.id())
        });

        Ok(())
    }

    /// Returns the tags of the addresses of the tracked native accounts.
    async fn tracked_account_tags(&self) -> Result<BTreeSet<NoteTag>, ClientError> {
        let tags = self
            .store
            .get_account_note_tags()
            .await?
            .into_iter()
            .map(|record| record.tag)
            .collect();
        Ok(tags)
    }

    /// Fetches bounded pages for request groups of at most
    /// [`Self::MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST`] tags.
    ///
    /// Each group starts from its lowest cursor. The import drops notes delivered again. Each tag
    /// keeps the higher of its stored cursor and the page cursor when their database nonces match.
    ///
    /// A failed page keeps the pages fetched before it and leaves the other groups to proceed.
    async fn fetch_transport_notes_in_chunks(
        &self,
    ) -> Result<NoteTransportLayerUpdate, ClientError> {
        let api = self.get_note_transport_api()?;
        let tags: Vec<_> = self.store.get_unique_note_tags().await?.into_iter().collect();
        let stored = self.load_note_transport_cursors().await?;
        let mut cursors: NoteTransportCursors = tags
            .iter()
            .filter_map(|tag| stored.get(tag).map(|cursor| (*tag, *cursor)))
            .collect();

        let mut update = NoteTransportLayerUpdate::default();
        let mut notes = Vec::new();
        for (start, group) in transport_request_groups(&tags, &cursors) {
            let mut cursor = start;
            for _ in 0..MAX_NOTE_TRANSPORT_PAGES_PER_GROUP {
                let page = match api
                    .fetch_notes_page(&group, cursor)
                    .await
                    .and_then(|page| validate_transport_page(page, &group, cursor))
                {
                    Ok(page) => page,
                    Err(error) => {
                        update.fetch_error.get_or_insert(error.into());
                        break;
                    },
                };
                for (id, note, block_hint) in page.notes {
                    update.id_by_commitment.insert(note.details_commitment(), id);
                    notes.push((id, note, block_hint));
                }
                cursor = page.cursor;
                for tag in &group {
                    let position = cursors.entry(*tag).or_insert(cursor);
                    *position = advance_transport_cursor(*position, cursor);
                }
                if !page.has_more {
                    break;
                }
            }
        }

        update.note_files = self.prepare_transport_notes(notes).await?;
        update.cursors = Some(cursors);
        Ok(update)
    }

    /// Screens fetched notes and prepares them for import with one set of store reads.
    async fn prepare_transport_notes(
        &self,
        mut notes: Vec<(NoteId, Note, Option<BlockNumber>)>,
    ) -> Result<Vec<NoteFile>, ClientError> {
        // Fallback lookback window, in blocks, used only for notes the transport delivered without
        // block information. Scanning back from sync height handles the race where a note is
        // committed on-chain just before the NTL delivers its data. Without it,
        // check_expected_notes would scan from sync_height forward and miss the already-committed
        // note. A transport-provided block is deterministic and always preferred.
        const NOTE_LOOKBACK_BLOCKS: u32 = 20;

        if notes.is_empty() {
            return Ok(Vec::new());
        }

        self.drop_notes_resolved_locally(&mut notes).await?;

        // Screen the transport-delivered notes to discard the ones that are not relevant to the
        // accounts tracked by the client. Boxed to avoid a `clippy::large_futures` warning, since
        // the sync future is already close to the size limit.
        Box::pin(self.screen_transport_notes(&mut notes)).await?;

        let sync_height = self.get_sync_height().await?;
        let fallback_after_block_num =
            BlockNumber::from(sync_height.as_u32().saturating_sub(NOTE_LOOKBACK_BLOCKS));

        let mut note_files = Vec::with_capacity(notes.len());
        for (_, note, block_hint) in notes {
            let tag = note.metadata().tag();
            // Prefer the transport-provided block, falling back to the lookback window when absent.
            let after_block_num = block_hint.unwrap_or(fallback_after_block_num);
            note_files.push(NoteFile::ExpectedNote {
                details: note.into(),
                sync_hint: NoteSyncHint::new(after_block_num, tag),
            });
        }

        Ok(note_files)
    }

    /// Fetches the notes the Note Transport Layer holds for the tracked tags.
    ///
    /// Fetches bounded pages for each group of tracked tags. This performs no node call and writes
    /// nothing, so it can run concurrently with the chain fetch. The caller imports the returned
    /// files and then persists all tag cursors. A failed request preserves successful pages in the
    /// returned update.
    ///
    /// Returns empty data when note transport is not configured.
    pub(crate) async fn fetch_note_transport_updates(
        &self,
    ) -> Result<NoteTransportLayerUpdate, ClientError> {
        if !self.is_note_transport_enabled() {
            return Ok(NoteTransportLayerUpdate::default());
        }

        self.fetch_transport_notes_in_chunks().await
    }

    /// Writes everything [`Client::fetch_note_transport_updates`] returned, in two steps:
    ///
    /// 1. Imports the fetched notes, which resolves their on-chain state and stores the records.
    /// 2. Saves the cursor for each tag.
    ///
    /// The notes are written before the cursors, so a crash between them re-fetches instead of
    /// skipping notes that were never written.
    ///
    /// Returns the ids of the imported notes and the details commitments of the records written.
    pub(crate) async fn apply_note_transport_update(
        &mut self,
        update: NoteTransportLayerUpdate,
    ) -> Result<(Vec<NoteId>, Vec<NoteDetailsCommitment>), ClientError> {
        let NoteTransportLayerUpdate {
            note_files,
            id_by_commitment,
            cursors,
            fetch_error,
        } = update;

        let written = self.import_notes(&note_files).await?;
        let mut imported_ids: Vec<NoteId> = written
            .iter()
            .filter_map(|commitment| id_by_commitment.get(commitment).copied())
            .collect();

        if let Some(cursors) = cursors {
            self.save_note_transport_cursors(&cursors).await?;
        }
        if let Some(error) = fetch_error {
            tracing::warn!(?error, "note transport fetch failed; saved successful pages for retry");
        }

        imported_ids.sort_unstable();
        imported_ids.dedup();

        Ok((imported_ids, written))
    }

    /// Drops deliveries of notes whose local record does not need them.
    ///
    /// A note that a local transaction is consuming cannot be overwritten, so its import would
    /// fail. A note that is already committed or consumed gains nothing from a second import and
    /// would cost a node request. A request from a group's lowest cursor can deliver such notes
    /// again. Expected, unverified, and invalid notes pass through because their records can need a
    /// new inclusion proof. A resolved record must match the ID in the transport header.
    async fn drop_notes_resolved_locally(
        &self,
        notes: &mut Vec<(NoteId, Note, Option<BlockNumber>)>,
    ) -> Result<(), ClientError> {
        let commitments = notes.iter().map(|(_, note, _)| note.details_commitment()).collect();
        let records: BTreeMap<_, _> = self
            .get_input_notes(NoteFilter::DetailsCommitments(commitments))
            .await?
            .into_iter()
            .map(|record| (record.details_commitment(), record))
            .collect();
        notes.retain(|(id, note, _)| {
            let Some(record) = records.get(&note.details_commitment()) else {
                return true;
            };
            if record.is_processing() {
                tracing::warn!(%id, "skipping delivery of a note being consumed locally");
                return false;
            }
            !((record.is_committed() || record.is_consumed()) && record.id() == Some(*id))
        });
        Ok(())
    }
}

// NOTE TRANSPORT FETCH
// ================================================================================================

/// What the note transport fetch returned, before anything is written.
///
/// Built by [`Client::fetch_note_transport_updates`] and consumed by
/// [`Client::apply_note_transport_update`].
#[derive(Default)]
pub(crate) struct NoteTransportLayerUpdate {
    /// Notes to import.
    note_files: Vec<NoteFile>,
    /// Note ids by details commitment, taken from the note headers the transport returned. Used to
    /// resolve the written records back to ids.
    id_by_commitment: BTreeMap<NoteDetailsCommitment, NoteId>,
    /// Cursor for each tag used by the steady-state fetch.
    cursors: Option<NoteTransportCursors>,
    /// First request error. Successful pages remain available for import.
    pub(crate) fetch_error: Option<ClientError>,
}

/// Splits the tracked tags into request groups and returns the cursor each group starts from.
///
/// Tags without a cursor start from the first retained note. Tags with a cursor are grouped by
/// database nonce, because sequences from different databases do not compare. Each nonce group is
/// sorted by sequence before it is split, so the tags in one request sit close together and the
/// lowest cursor limits repeated deliveries.
fn transport_request_groups(
    tags: &[NoteTag],
    cursors: &NoteTransportCursors,
) -> Vec<(NoteTransportCursor, Vec<NoteTag>)> {
    let mut by_nonce = BTreeMap::<Option<u64>, Vec<(NoteTransportCursor, NoteTag)>>::new();
    for tag in tags {
        let cursor = cursors.get(tag).copied().unwrap_or_else(NoteTransportCursor::init);
        by_nonce
            .entry(cursor.parts().map(|(nonce, _)| nonce))
            .or_default()
            .push((cursor, *tag));
    }

    let mut groups = Vec::new();
    for mut entries in by_nonce.into_values() {
        entries.sort_unstable();
        for chunk in entries.chunks(MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST) {
            let start = chunk[0].0;
            groups.push((start, chunk.iter().map(|(_, tag)| *tag).collect()));
        }
    }
    groups
}

/// Returns the position of a tag after a page that ends at `page_cursor`.
///
/// A page from the same database never moves a tag backwards. A page from a different database
/// replaces a position that database does not recognize.
fn advance_transport_cursor(
    current: NoteTransportCursor,
    page_cursor: NoteTransportCursor,
) -> NoteTransportCursor {
    match (current.parts(), page_cursor.parts()) {
        (Some((nonce, sequence)), Some((page_nonce, page_sequence))) if nonce == page_nonce => {
            NoteTransportCursor::from_parts(nonce, sequence.max(page_sequence))
        },
        _ => page_cursor,
    }
}

struct ValidatedTransportPage {
    notes: Vec<(NoteId, Note, Option<BlockNumber>)>,
    cursor: NoteTransportCursor,
    has_more: bool,
}

/// Validates a complete page before its notes or cursor can advance local progress.
fn validate_transport_page(
    page: NoteTransportPage,
    tags: &[NoteTag],
    request_cursor: NoteTransportCursor,
) -> Result<ValidatedTransportPage, NoteTransportError> {
    let Some((nonce, sequence)) = page.cursor.parts() else {
        return Err(NoteTransportError::Network(String::from("fetch response has no cursor")));
    };
    if request_cursor.parts().is_some_and(|(request_nonce, request_sequence)| {
        nonce == request_nonce
            && (sequence < request_sequence
                || (!page.notes.is_empty() && sequence == request_sequence))
    }) || (!page.notes.is_empty() && sequence == 0)
        || (page.has_more && page.notes.is_empty())
    {
        return Err(NoteTransportError::Network(String::from(
            "fetch response has invalid pagination progress",
        )));
    }
    let mut notes = Vec::with_capacity(page.notes.len());
    for info in page.notes {
        let note = rejoin_note(&info.header, &info.details_bytes)?;
        if !tags.contains(&note.metadata().tag()) {
            return Err(NoteTransportError::UnrequestedTag(note.metadata().tag()));
        }
        // The header ID includes attachments that the transport does not send.
        notes.push((info.header.id(), note, info.block_hint));
    }
    Ok(ValidatedTransportPage {
        notes,
        cursor: page.cursor,
        has_more: page.has_more,
    })
}

/// The part of a note that the note transport network sends to a recipient.
///
/// The transport sends the original header and details. It does not send note attachments. The
/// constructor verifies that the header commits to the details.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportNote {
    header: NoteHeader,
    details: NoteDetails,
}

impl TransportNote {
    /// Creates a transport note from matching note parts.
    pub fn new(header: NoteHeader, details: NoteDetails) -> Result<Self, NoteTransportError> {
        validate_note_parts(&header, &details)?;
        Ok(Self { header, details })
    }

    /// Returns the note header.
    pub fn header(&self) -> &NoteHeader {
        &self.header
    }

    /// Returns the note details.
    pub fn details(&self) -> &NoteDetails {
        &self.details
    }

    /// Returns the note header and details.
    pub fn into_parts(self) -> (NoteHeader, NoteDetails) {
        (self.header, self.details)
    }
}

impl From<Note> for TransportNote {
    fn from(note: Note) -> Self {
        let header = *note.header();
        let details = NoteDetails::from(note);
        Self { header, details }
    }
}

/// The main transport client trait for sending and receiving private notes.
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait NoteTransportClient: Send + Sync {
    /// Sends a note together with its inclusion proof.
    ///
    /// The transport carries the proof to the network. The network verifies `inclusion_proof`
    /// before it stores the note and relays the exact commitment block to the recipient.
    async fn send_note_with_proof(
        &self,
        note: TransportNote,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), NoteTransportError>;

    /// Fetches notes for the given tags.
    ///
    /// Downloads notes for the given tags. Returns notes after the provided cursor (pagination),
    /// and an updated cursor.
    async fn fetch_notes(
        &self,
        tag: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError>;

    /// Fetches a page and reports whether another page is available.
    ///
    /// Transports without a continuation flag use an empty page to confirm the end of the history.
    async fn fetch_notes_page(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<NoteTransportPage, NoteTransportError> {
        let (notes, cursor) = self.fetch_notes(tags, cursor).await?;
        let has_more = !notes.is_empty();
        Ok(NoteTransportPage { notes, cursor, has_more })
    }
}

/// A page of notes from the transport service.
pub struct NoteTransportPage {
    /// Notes in global sequence order.
    pub notes: Vec<NoteInfo>,
    /// Position of the last returned note. An empty page keeps the request position.
    pub cursor: NoteTransportCursor,
    /// Indicates that the requested tags have another page.
    pub has_more: bool,
}

/// Information about a note fetched from the note transport network
#[derive(Debug, Clone)]
pub struct NoteInfo {
    /// Note header.
    pub header: NoteHeader,
    /// Serialized note details.
    pub details_bytes: Vec<u8>,
    /// Block from which the recipient starts scanning for the note's on-chain commitment. This is
    /// either an unverified sender hint or the exact block verified by a proof-aware transport.
    /// `None` applies the recipient's default lookback window.
    pub block_hint: Option<BlockNumber>,
}

impl NoteInfo {
    /// Builds a [`NoteInfo`] without a block hint (`block_hint` is `None`).
    ///
    /// Use the [`NoteInfo::block_hint`] field directly to attach a hint.
    pub fn new(header: NoteHeader, details_bytes: Vec<u8>) -> Self {
        Self { header, details_bytes, block_hint: None }
    }
}

// SERIALIZATION
// ================================================================================================

impl Serializable for TransportNote {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.header.write_into(target);
        self.details.to_bytes().write_into(target);
    }
}

impl Deserializable for TransportNote {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let header = NoteHeader::read_from(source)?;
        let details_bytes = Vec::<u8>::read_from(source)?;
        let details = NoteDetails::read_from_bytes(&details_bytes)?;
        Self::new(header, details)
            .map_err(|error| DeserializationError::InvalidValue(format!("{error}")))
    }
}

impl Serializable for NoteInfo {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.header.write_into(target);
        self.details_bytes.write_into(target);
        self.block_hint.write_into(target);
    }
}

impl Deserializable for NoteInfo {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let header = NoteHeader::read_from(source)?;
        let details_bytes = Vec::<u8>::read_from(source)?;
        let block_hint = Option::<BlockNumber>::read_from(source)?;
        Ok(NoteInfo { header, details_bytes, block_hint })
    }
}

fn rejoin_note(header: &NoteHeader, details_bytes: &[u8]) -> Result<Note, NoteTransportError> {
    let mut reader = SliceReader::new(details_bytes);
    let details = NoteDetails::read_from(&mut reader)?;
    validate_note_parts(header, &details)?;
    // The transport wire format only carries `NoteHeader` + serialized `NoteDetails`, not the
    // attachments collection. We rejoin with empty attachments; this matches the original note only
    // when it had no attachments in the first place.
    let partial_metadata = *header.metadata().partial_metadata();
    Ok(Note::new(
        details.assets().clone(),
        partial_metadata,
        details.recipient().clone(),
    ))
}

/// Checks that the note header commits to the supplied details.
pub(crate) fn validate_note_parts(
    header: &NoteHeader,
    details: &NoteDetails,
) -> Result<(), NoteTransportError> {
    let header_commitment = header.details_commitment();
    let details_commitment = details.commitment();
    if header_commitment != details_commitment {
        return Err(NoteTransportError::NoteDetailsMismatch {
            header: header_commitment,
            details: details_commitment,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_rejects_invalid_pagination() {
        let cursor = NoteTransportCursor::from_parts(1, 10);
        for (returned, has_more) in [
            (NoteTransportCursor::init(), false),
            (NoteTransportCursor::from_parts(1, 9), false),
            (cursor, true),
        ] {
            let page = NoteTransportPage {
                notes: Vec::new(),
                cursor: returned,
                has_more,
            };
            assert!(validate_transport_page(page, &[], cursor).is_err());
        }
    }
}

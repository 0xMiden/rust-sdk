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
use crate::sync::NoteTagSource;
use crate::{Client, ClientError};

pub const NOTE_TRANSPORT_MAINNET_ENDPOINT: &str = "https://transport.mainnet.miden.io";
pub const NOTE_TRANSPORT_TESTNET_ENDPOINT: &str = "https://transport.miden.io";
pub const NOTE_TRANSPORT_DEVNET_ENDPOINT: &str = "https://transport.devnet.miden.io";
/// Settings key for the unused aggregate transport cursor.
#[deprecated(since = "0.17.1", note = "note transport does not read this setting")]
pub const NOTE_TRANSPORT_CURSOR_STORE_SETTING: &str = "note_transport_cursor";
/// Legacy settings key for the cursor of each tag. The client migrates this value to
/// [`NOTE_TRANSPORT_SYNC_STATE_KEY`].
#[deprecated(
    since = "0.18.0",
    note = "note transport stores one cursor under NOTE_TRANSPORT_SYNC_STATE_KEY"
)]
pub const NOTE_TRANSPORT_CURSORS_KEY: &str = "note_transport_cursors";
/// Settings key for the note transport cursor and the tags that it covers.
pub const NOTE_TRANSPORT_SYNC_STATE_KEY: &str = "note_transport_sync_state";

/// Maximum number of note tags in one transport fetch request. The service rejects larger requests.
const MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST: usize = 128;

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

    /// Loads the note transport sync state.
    ///
    /// A store without the state migrates the legacy cursor of each tag. A missing or unreadable
    /// value returns `None`, so the next fetch reads the retained history of all tags.
    #[allow(deprecated)]
    async fn load_note_transport_sync_state(
        &self,
    ) -> Result<Option<NoteTransportSyncState>, ClientError> {
        let get = |key: &str| self.store.get_setting(SettingScope::Client, key.into());
        let state = if let Some(bytes) = get(NOTE_TRANSPORT_SYNC_STATE_KEY).await? {
            NoteTransportSyncState::read_from_bytes(&bytes).map(Some)
        } else if let Some(bytes) = get(NOTE_TRANSPORT_CURSORS_KEY).await? {
            BTreeMap::<NoteTag, Option<NoteTransportCursor>>::read_from_bytes(&bytes)
                .map(migrate_tag_cursors)
        } else {
            Ok(None)
        };
        Ok(state
            .inspect_err(|err| tracing::warn!(?err, "resetting unreadable note transport state"))
            .unwrap_or_default())
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
    /// A failed request returns an error after the fetched notes are imported, and the next call
    /// fetches the failed tags again.
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

    /// Returns the tracked tags that were registered for an account, i.e. derived from its ID.
    async fn tracked_account_tags(&self) -> Result<BTreeSet<NoteTag>, ClientError> {
        let tags = self
            .store
            .get_note_tags()
            .await?
            .into_iter()
            .filter(|record| matches!(record.source, NoteTagSource::Account(_)))
            .map(|record| record.tag)
            .collect();
        Ok(tags)
    }

    /// Fetches the notes of all tracked tags up to the bound of the first page, with covered tags
    /// from the stored cursor and the other tags from the first retained note.
    async fn fetch_transport_notes_in_chunks(
        &self,
    ) -> Result<NoteTransportLayerUpdate, ClientError> {
        let api = self.get_note_transport_api()?;
        let (start, stored_tags) = self.load_note_transport_sync_state().await?.unzip();
        let (covered, added): (Vec<_>, Vec<_>) = self
            .store
            .get_unique_note_tags()
            .await?
            .into_iter()
            .partition(|tag| stored_tags.as_ref().is_some_and(|tags| tags.contains(tag)));

        let mut update = NoteTransportLayerUpdate::default();
        let mut notes = Vec::new();
        let mut sync_to = None;
        let mut reached = BTreeSet::new();
        let chunks = covered
            .chunks(MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST)
            .map(|chunk| (start, chunk))
            .chain(added.chunks(MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST).map(|chunk| (None, chunk)));
        for (start, chunk) in chunks {
            match fetch_transport_chunk(api.as_ref(), chunk, start, &mut sync_to, &mut notes).await
            {
                Ok(()) => reached.extend(chunk),
                Err(error) => {
                    update.fetch_error.get_or_insert(error.into());
                },
            }
        }

        // A covered tag that did not reach the bound would skip notes if the cursor moved.
        if let Some(cursor) = sync_to
            && covered.iter().all(|tag| reached.contains(tag))
        {
            update.sync_state = Some((cursor, reached));
        }
        update.id_by_commitment =
            notes.iter().map(|(id, note, _)| (note.details_commitment(), *id)).collect();
        update.note_files = self.prepare_transport_notes(notes).await?;
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
    /// Fetches the notes of all tracked tags up to one bound. This performs no node call and writes
    /// nothing, so it can run concurrently with the chain fetch. The caller imports the returned
    /// files and then saves the sync state. A failed request keeps the notes of earlier pages in
    /// the returned update.
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
    /// 2. Saves the sync state.
    ///
    /// The notes are written before the sync state, so a crash between them re-fetches instead of
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
            sync_state,
            fetch_error,
        } = update;

        let written = self.import_notes(&note_files).await?;
        let mut imported_ids: Vec<NoteId> = written
            .iter()
            .filter_map(|commitment| id_by_commitment.get(commitment).copied())
            .collect();

        if let Some(state) = sync_state {
            let key = String::from(NOTE_TRANSPORT_SYNC_STATE_KEY);
            self.store.set_setting(SettingScope::Client, key, state.to_bytes()).await?;
        }
        if let Some(error) = fetch_error {
            tracing::warn!(
                ?error,
                "note transport fetch failed; the next fetch reads the tags again"
            );
        }

        imported_ids.sort_unstable();
        imported_ids.dedup();

        Ok((imported_ids, written))
    }

    /// Drops deliveries of notes whose local record does not need them.
    ///
    /// A note that a local transaction is consuming cannot be overwritten, so its import would
    /// fail. A note that is already committed or consumed gains nothing from a second import and
    /// would cost a node request. A tag that starts from the first retained note, or a fetch after
    /// a failed fetch, can deliver such notes again. Expected, unverified, and invalid notes pass
    /// through because their records can need a new inclusion proof. A resolved record must match
    /// the ID in the transport header.
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
    /// New sync state. `None` keeps the stored state.
    sync_state: Option<NoteTransportSyncState>,
    /// First request error. Successful pages remain available for import.
    pub(crate) fetch_error: Option<ClientError>,
}

/// Stored cursor and the tags that received all notes up to it.
type NoteTransportSyncState = (NoteTransportCursor, BTreeSet<NoteTag>);

/// Migrates the legacy cursor of each tag to the lowest cursor.
///
/// Sequences from different service databases do not compare, so cursors with different nonces
/// migrate to no state. A tag without a cursor is not covered.
fn migrate_tag_cursors(
    cursors: BTreeMap<NoteTag, Option<NoteTransportCursor>>,
) -> Option<NoteTransportSyncState> {
    let cursors: BTreeMap<_, _> =
        cursors.into_iter().filter_map(|(tag, cursor)| Some((tag, cursor?))).collect();
    let cursor = *cursors.values().min_by_key(|cursor| cursor.sequence)?;
    cursors
        .values()
        .all(|other| other.nonce == cursor.nonce)
        .then(|| (cursor, cursors.into_keys().collect()))
}

/// Fetches the pages of one chunk of tags after `start` and up to the bound.
///
/// The first successful page sets the bound when `sync_to` has none.
async fn fetch_transport_chunk(
    api: &dyn NoteTransportClient,
    tags: &[NoteTag],
    start: Option<NoteTransportCursor>,
    sync_to: &mut Option<NoteTransportCursor>,
    notes: &mut Vec<(NoteId, Note, Option<BlockNumber>)>,
) -> Result<(), NoteTransportError> {
    let mut cursor = start;
    // Validation requires each page to move the cursor toward the fixed bound, so the loop ends.
    loop {
        let page = api.fetch_notes_page(tags, cursor, *sync_to).await?;
        notes.extend(validate_transport_page(&page, tags, cursor, *sync_to)?);
        sync_to.get_or_insert(page.sync_to);
        if !page.has_more {
            return Ok(());
        }
        cursor = Some(page.cursor);
    }
}

/// Validates a complete page before its notes or cursor can advance local progress, and returns its
/// notes.
fn validate_transport_page(
    page: &NoteTransportPage,
    tags: &[NoteTag],
    request_cursor: Option<NoteTransportCursor>,
    request_sync_to: Option<NoteTransportCursor>,
) -> Result<Vec<(NoteId, Note, Option<BlockNumber>)>, NoteTransportError> {
    let NoteTransportCursor { nonce, sequence } = page.cursor;
    if request_sync_to.is_some_and(|bound| bound != page.sync_to)
        || nonce != page.sync_to.nonce
        || sequence > page.sync_to.sequence
    {
        return Err(NoteTransportError::Network(String::from(
            "fetch response does not match the sync bound",
        )));
    }
    if request_cursor.is_some_and(|request| {
        nonce == request.nonce
            && (sequence < request.sequence
                || (!page.notes.is_empty() && sequence == request.sequence))
    }) || (!page.notes.is_empty() && sequence == 0)
        || (page.has_more && page.notes.is_empty())
    {
        return Err(NoteTransportError::Network(String::from(
            "fetch response has invalid pagination progress",
        )));
    }
    let mut notes = Vec::with_capacity(page.notes.len());
    for info in &page.notes {
        let note = rejoin_note(&info.header, &info.details_bytes)?;
        if !tags.contains(&note.metadata().tag()) {
            return Err(NoteTransportError::UnrequestedTag(note.metadata().tag()));
        }
        // The header ID includes attachments that the transport does not send.
        notes.push((info.header.id(), note, info.block_hint));
    }
    Ok(notes)
}

/// Note transport cursor
///
/// Identifies a position in the note transport service's stored-note sequence.
///
/// The sequence is global across tags in one service database. Compare sequences only when their
/// nonces match.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NoteTransportCursor {
    /// Identifies the service database.
    pub nonce: u64,
    /// Position in the stored-note sequence of the service database.
    pub sequence: u64,
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

    /// Fetches a page of notes after `cursor` and up to `sync_to` for the given tags.
    ///
    /// Without `cursor`, the page starts from the first retained note. Without `sync_to`, the
    /// service uses its current head as the bound. The page returns the bound that the service
    /// used. A cursor from another service database restarts from the first retained note.
    async fn fetch_notes_page(
        &self,
        tags: &[NoteTag],
        cursor: Option<NoteTransportCursor>,
        sync_to: Option<NoteTransportCursor>,
    ) -> Result<NoteTransportPage, NoteTransportError>;
}

/// A page of notes from the transport service.
pub struct NoteTransportPage {
    /// Notes in global sequence order.
    pub notes: Vec<NoteInfo>,
    /// Position of the last returned note. An empty page keeps the request position.
    pub cursor: NoteTransportCursor,
    /// Indicates that the requested tags have another page up to `sync_to`.
    pub has_more: bool,
    /// Inclusive upper bound of the request. When `has_more` is `false`, the page completes the
    /// requested tags up to this bound.
    pub sync_to: NoteTransportCursor,
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

impl Serializable for NoteTransportCursor {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        (self.nonce, self.sequence).write_into(target);
    }
}

impl Deserializable for NoteTransportCursor {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let (nonce, sequence) = <(u64, u64)>::read_from(source)?;
        Ok(Self { nonce, sequence })
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

    fn cursor(sequence: u64) -> NoteTransportCursor {
        NoteTransportCursor { nonce: 1, sequence }
    }

    fn empty_page(cursor: NoteTransportCursor, has_more: bool) -> NoteTransportPage {
        NoteTransportPage {
            notes: Vec::new(),
            cursor,
            has_more,
            sync_to: self::cursor(20),
        }
    }

    #[test]
    fn transport_rejects_invalid_pagination() {
        for page in [empty_page(cursor(9), false), empty_page(cursor(10), true)] {
            assert!(validate_transport_page(&page, &[], Some(cursor(10)), None).is_err());
        }
    }

    #[test]
    fn transport_rejects_page_outside_sync_bound() {
        let other_database = NoteTransportCursor { nonce: 2, sequence: 5 };
        for (page, request_sync_to) in [
            // The page changes the requested bound.
            (empty_page(cursor(10), false), Some(cursor(21))),
            // The page cursor passes the bound.
            (empty_page(cursor(21), false), None),
            // The page cursor and the bound come from different service databases.
            (empty_page(other_database, false), None),
        ] {
            assert!(
                validate_transport_page(&page, &[], Some(cursor(10)), request_sync_to).is_err()
            );
        }
    }

    #[test]
    fn transport_accepts_pages_within_sync_bound() {
        let page = empty_page(cursor(10), false);
        assert!(validate_transport_page(&page, &[], Some(cursor(10)), Some(cursor(20))).is_ok());

        // A cursor from another service database restarts from the first retained note.
        let other_database = NoteTransportCursor { nonce: 2, sequence: 50 };
        let page = empty_page(cursor(3), false);
        assert!(validate_transport_page(&page, &[], Some(other_database), None).is_ok());
    }

    #[test]
    fn transport_reads_legacy_tag_cursors() {
        let tag = NoteTag::new(1);
        let legacy = BTreeMap::from([(tag, Some((1u64, 4u64)))]).to_bytes();

        let cursors =
            BTreeMap::<NoteTag, Option<NoteTransportCursor>>::read_from_bytes(&legacy).unwrap();

        assert_eq!(cursors, BTreeMap::from([(tag, Some(cursor(4)))]));
    }

    #[test]
    fn transport_migrates_tag_cursors_to_lowest_cursor() {
        let [first, second, third] = [1, 2, 3].map(NoteTag::new);
        let cursors =
            BTreeMap::from([(first, Some(cursor(7))), (second, Some(cursor(4))), (third, None)]);

        let state = migrate_tag_cursors(cursors);

        assert_eq!(state, Some((cursor(4), BTreeSet::from([first, second]))));
    }

    #[test]
    fn transport_migration_drops_cursors_from_different_databases() {
        let other_database = NoteTransportCursor { nonce: 2, sequence: 4 };
        let cursors = BTreeMap::from([
            (NoteTag::new(1), Some(cursor(7))),
            (NoteTag::new(2), Some(other_database)),
        ]);

        assert_eq!(migrate_tag_cursors(cursors), None);
        assert_eq!(migrate_tag_cursors(BTreeMap::new()), None);
    }
}

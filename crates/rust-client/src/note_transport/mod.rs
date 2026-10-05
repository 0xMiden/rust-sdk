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
use crate::store::{InputNoteRecord, NoteFilter, SettingScope};
use crate::sync::NoteTagSource;
use crate::{Client, ClientError};

pub const NOTE_TRANSPORT_MAINNET_ENDPOINT: &str = "https://transport.mainnet.miden.io";
pub const NOTE_TRANSPORT_TESTNET_ENDPOINT: &str = "https://transport.miden.io";
pub const NOTE_TRANSPORT_DEVNET_ENDPOINT: &str = "https://transport.devnet.miden.io";
/// Settings key for the unused aggregate transport cursor.
#[deprecated(since = "0.17.1", note = "note transport stores a cursor for each tag")]
pub const NOTE_TRANSPORT_CURSOR_STORE_SETTING: &str = "note_transport_cursor";
pub const NOTE_TRANSPORT_CURSORS_KEY: &str = "note_transport_cursors";

type NoteTransportCursors = BTreeMap<NoteTag, NoteTransportCursor>;
// Bound each chunk independently so one large history cannot prevent other chunks from progressing.
const MAX_NOTE_TRANSPORT_PAGES_PER_CHUNK: usize = 32;

/// Legacy settings key for note transport backfill state.
#[deprecated(since = "0.17.1", note = "note transport no longer keeps per-tag backfill state")]
pub const NOTE_TRANSPORT_COVERED_TAGS_KEY: &str = "note_transport_covered_tags";

/// Settings key for the durable relay outbox: a serialized `Vec<RelayOutboxEntry>` of private notes
/// whose transport delivery has not yet succeeded. [`Client::send_private_note_with_proof`] appends
/// (replacing any entry with the same note id) before relaying; [`Client::flush_relay_outbox`]
/// drains entries that re-send successfully. Reusing the settings k/v avoids a Store-trait schema
/// change while surviving process restarts.
pub const NOTE_TRANSPORT_OUTBOX_KEY: &str = "note_transport_outbox";

/// Client note transport methods.
impl<AUTH> Client<AUTH> {
    /// Maximum number of note tags in one transport fetch request.
    pub const MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST: usize = 128;

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
    /// **Durability.** The note and its proof are persisted to the outbox before the transport
    /// call. If the call fails or is interrupted, the entry stays in the outbox and is retried on
    /// the next [`Client::flush_relay_outbox`] (which [`Client::sync_note_transport`] runs), so a
    /// transient transport failure does not drop the note. The receiver dedupes by note id, so a
    /// re-send after a partial success is harmless.
    pub async fn send_private_note_with_proof(
        &mut self,
        note: Note,
        address: &Address,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), ClientError> {
        let api = self.get_note_transport_api()?;

        let note = TransportNote::from(note);
        let note_id = note.header().id();
        // The address is reserved for end-to-end encryption of the note details:
        // address.key().encrypt(note.details().to_bytes()).
        let _ = address;

        // Persist the payload before the network call so a failed or interrupted send leaves a
        // recoverable record rather than losing the only copy with the call frame. The proof
        // travels with the entry so a retried send relays the same value.
        let entry = RelayOutboxEntry { note, inclusion_proof };
        let mut outbox = self.load_relay_outbox().await?;
        // Replace any existing entry for this note id so the latest payload wins when a
        // still-pending note is re-sent.
        outbox.retain(|e| e.note.header().id() != note_id);
        outbox.push(entry.clone());
        self.save_relay_outbox(outbox).await?;

        entry.relay(api.as_ref()).await?;

        // Relay succeeded — drop the entry. A failed store write here is tolerable: the next flush
        // re-sends and the receiver dedupes by note id, so a stale entry never causes loss.
        let mut outbox = self.load_relay_outbox().await?;
        outbox.retain(|e| e.note.header().id() != note_id);
        self.save_relay_outbox(outbox).await?;

        Ok(())
    }

    /// Re-attempt every relay payload in the durable outbox. Each entry is a private note whose
    /// previous transport delivery failed. Successful re-sends are dropped; failures are kept for
    /// the next call. Every entry is attempted independently, so one persistently-failing note does
    /// not block the others.
    ///
    /// [`Client::sync_note_transport`] runs this automatically and ignores its error, so a relay
    /// failure can't block a sync. Callers driving retries themselves can invoke it directly and
    /// inspect the returned error.
    pub async fn flush_relay_outbox(&self) -> Result<(), ClientError> {
        let api = self.get_note_transport_api()?;

        let entries = self.load_relay_outbox().await?;
        if entries.is_empty() {
            return Ok(());
        }

        // Attempt every entry independently so a single persistently-failing note can't block the
        // rest. The outbox holds only the caller's own failed sends, so it stays small and this is
        // not a meaningful burst.
        let mut remaining = Vec::new();
        let mut last_err: Option<NoteTransportError> = None;

        for entry in entries {
            match entry.relay(api.as_ref()).await {
                Ok(()) => {},
                Err(err) => {
                    tracing::warn!(?err, "relay-outbox entry retry failed; will retry next sync");
                    remaining.push(entry);
                    last_err = Some(err);
                },
            }
        }

        self.save_relay_outbox(remaining).await?;

        if let Some(err) = last_err {
            return Err(err.into());
        }
        Ok(())
    }

    /// Load the durable relay outbox.
    ///
    /// Returns an empty `Vec` if the outbox key is absent. On deserialization failure (schema
    /// mismatch or storage corruption) the entry is dropped and an empty `Vec` is returned —
    /// leaving unreadable bytes in place would block every subsequent relay because each sync would
    /// re-read them.
    async fn load_relay_outbox(&self) -> Result<Vec<RelayOutboxEntry>, ClientError> {
        let bytes = self
            .store
            .get_setting(SettingScope::Client, String::from(NOTE_TRANSPORT_OUTBOX_KEY))
            .await
            .map_err(ClientError::StoreError)?;
        let Some(bytes) = bytes else {
            return Ok(Vec::new());
        };
        match Vec::<RelayOutboxEntry>::read_from_bytes(&bytes) {
            Ok(entries) => Ok(entries),
            Err(err) => {
                tracing::warn!(?err, "dropping unreadable relay outbox; resetting to empty");
                self.store
                    .remove_setting(SettingScope::Client, String::from(NOTE_TRANSPORT_OUTBOX_KEY))
                    .await
                    .map_err(ClientError::StoreError)?;
                Ok(Vec::new())
            },
        }
    }

    /// Persist the relay outbox, removing the key entirely when empty so the settings table doesn't
    /// accumulate empty-vec blobs.
    async fn save_relay_outbox(&self, entries: Vec<RelayOutboxEntry>) -> Result<(), ClientError> {
        let key = String::from(NOTE_TRANSPORT_OUTBOX_KEY);
        if entries.is_empty() {
            self.store
                .remove_setting(SettingScope::Client, key)
                .await
                .map_err(ClientError::StoreError)?;
            return Ok(());
        }
        let bytes = entries.to_bytes();
        self.store
            .set_setting(SettingScope::Client, key, bytes)
            .await
            .map_err(ClientError::StoreError)
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
            self.store
                .remove_setting(SettingScope::Client, key)
                .await
                .map_err(ClientError::StoreError)?;
            return Ok(());
        }

        self.store
            .set_setting(SettingScope::Client, key, cursors.to_bytes())
            .await
            .map_err(ClientError::StoreError)
    }

    /// Returns the unique tracked tags that the note transport layer must fetch.
    async fn transport_note_tags(&self) -> Result<Vec<NoteTag>, ClientError> {
        Ok(self.store.get_unique_note_tags().await?.into_iter().collect())
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
    /// An internal pagination mechanism fetches only notes past the stored cursor for each tag.
    /// Tags without a stored cursor start from the initial cursor.
    ///
    /// A failed request returns an error after successful pages are imported and their cursors are
    /// saved. Histories that exceed the page budget continue on the next call.
    pub async fn fetch_private_notes(&mut self) -> Result<(), ClientError> {
        self.ensure_genesis_in_place().await?;

        let note_tags = self.transport_note_tags().await?;
        let mut update = self.fetch_transport_notes_in_chunks(&note_tags).await?;
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
        notes: &mut Vec<(Note, Option<BlockNumber>)>,
    ) -> Result<(), ClientError> {
        let account_tags = self.tracked_account_tags().await?;

        let notes_to_screen: Vec<Note> = notes
            .iter()
            .filter(|(note, _)| account_tags.contains(&note.metadata().tag()))
            .map(|(note, _)| note.clone())
            .collect();
        let consumable = self.note_screener().get_batch_consumability(&notes_to_screen).await?;

        // Discard the notes whose tag match the tracked accounts but are not consumable.
        notes.retain(|(note, _)| {
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

    /// Fetches bounded pages for groups of at most 128 tags at the same cursor.
    ///
    /// Regrouping uses a target known before the fetch. The furthest cursor preserves progress for
    /// the next sync. Failed pages leave successful pages available for import.
    async fn fetch_transport_notes_in_chunks(
        &self,
        tags: &[NoteTag],
    ) -> Result<NoteTransportLayerUpdate, ClientError> {
        let api = self.get_note_transport_api()?;
        let stored = self.load_note_transport_cursors().await?;
        let mut cursors: NoteTransportCursors = tags
            .iter()
            .filter_map(|tag| stored.get(tag).map(|cursor| (*tag, *cursor)))
            .collect();
        let targets = max_transport_sequences(&cursors);
        let mut groups = BTreeMap::<NoteTransportCursor, Vec<NoteTag>>::new();
        for tag in tags {
            let cursor = cursors.get(tag).copied().unwrap_or_else(NoteTransportCursor::init);
            groups.entry(cursor).or_default().push(*tag);
        }

        let mut update = NoteTransportLayerUpdate::default();
        let mut notes = Vec::new();
        let mut caught_up = BTreeSet::new();
        for (cursor, group) in groups {
            for chunk in group.chunks(Self::MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST) {
                let mut cursor = cursor;
                for page_index in 0..MAX_NOTE_TRANSPORT_PAGES_PER_CHUNK {
                    let page = match api
                        .fetch_notes_page(chunk, cursor)
                        .await
                        .and_then(|page| validate_transport_page(page, chunk, cursor))
                    {
                        Ok(page) => page,
                        Err(error) => {
                            update.fetch_error.get_or_insert(error.into());
                            break;
                        },
                    };
                    for (id, note, block_hint) in page.notes {
                        update.id_by_commitment.insert(note.details_commitment(), id);
                        notes.push((note, block_hint));
                    }
                    cursor = page.cursor;
                    cursors.extend(chunk.iter().map(|tag| (*tag, cursor)));
                    if !page.has_more {
                        caught_up.extend(chunk.iter().copied());
                        break;
                    }
                    if page_index + 1 == MAX_NOTE_TRANSPORT_PAGES_PER_CHUNK {
                        tracing::warn!(tags = ?chunk, "note transport page budget exhausted; retaining progress for the next sync");
                    }
                }
            }
        }

        // Keep the furthest cursor so the next target can advance. Other covered tags share the
        // target known before the requests. An earlier empty response cannot prove coverage of a
        // sequence learned from a later response.
        let leaders = max_transport_sequences(&cursors);
        for (tag, cursor) in &mut cursors {
            if let Some((nonce, sequence)) = cursor.parts()
                && let Some(&target) = targets.get(&nonce)
                && (caught_up.contains(tag) || sequence >= target)
                && (sequence <= target || sequence < leaders[&nonce])
            {
                *cursor = NoteTransportCursor::from_parts(nonce, target);
            }
        }

        update.note_files = self.prepare_transport_notes(notes).await?;
        update.cursors = Some(cursors);
        Ok(update)
    }

    /// Screens fetched notes and prepares them for import with one set of store reads.
    async fn prepare_transport_notes(
        &self,
        mut notes: Vec<(Note, Option<BlockNumber>)>,
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

        // Screen the transport-delivered notes to discard the ones that are not relevant to the
        // accounts tracked by the client. Boxed to avoid a `clippy::large_futures` warning, since
        // the sync future is already close to the size limit.
        Box::pin(self.screen_transport_notes(&mut notes)).await?;

        self.drop_notes_processed_locally(&mut notes).await?;

        let sync_height = self.get_sync_height().await?;
        let fallback_after_block_num =
            BlockNumber::from(sync_height.as_u32().saturating_sub(NOTE_LOOKBACK_BLOCKS));

        let mut note_files = Vec::with_capacity(notes.len());
        for (note, block_hint) in notes {
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
    /// nothing but the relay outbox, so it can run concurrently with the chain fetch. The caller
    /// imports the returned files and then persists all tag cursors. A failed request preserves
    /// successful pages in the returned update.
    ///
    /// Returns empty data when note transport is not configured.
    pub(crate) async fn fetch_note_transport_updates(
        &self,
    ) -> Result<NoteTransportLayerUpdate, ClientError> {
        if !self.is_note_transport_enabled() {
            return Ok(NoteTransportLayerUpdate::default());
        }

        // Drain any private notes whose previous relay attempt failed. A flush error is logged, not
        // propagated: a failing relay must not block the sync, and the entries stay durable for the
        // next attempt. This is the one write this phase performs; it touches only the outbox
        // setting, which is independent of everything the apply phase writes.
        if let Err(err) = self.flush_relay_outbox().await {
            tracing::warn!(?err, "relay outbox flush failed during sync; entries retained");
        }

        let note_tags = self.transport_note_tags().await?;
        self.fetch_transport_notes_in_chunks(&note_tags).await
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

    /// Drops deliveries of notes a local transaction is consuming; importing them would fail on the
    /// no-overwrite-while-processing guard.
    async fn drop_notes_processed_locally(
        &self,
        notes: &mut Vec<(Note, Option<BlockNumber>)>,
    ) -> Result<(), ClientError> {
        if notes.is_empty() {
            return Ok(());
        }

        let commitments = notes.iter().map(|(note, _)| note.details_commitment()).collect();
        let processing: BTreeSet<NoteDetailsCommitment> = self
            .get_input_notes(NoteFilter::DetailsCommitments(commitments))
            .await?
            .into_iter()
            .filter(InputNoteRecord::is_processing)
            .map(|record| record.details_commitment())
            .collect();

        if !processing.is_empty() {
            tracing::warn!(?processing, "skipping deliveries of notes being consumed locally");
            notes.retain(|(note, _)| !processing.contains(&note.details_commitment()));
        }
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

/// Returns the furthest sequence for each database nonce.
fn max_transport_sequences(cursors: &NoteTransportCursors) -> BTreeMap<u64, u64> {
    let mut sequences = BTreeMap::<u64, u64>::new();
    for cursor in cursors.values() {
        if let Some((nonce, sequence)) = cursor.parts() {
            sequences
                .entry(nonce)
                .and_modify(|maximum| *maximum = (*maximum).max(sequence))
                .or_insert(sequence);
        }
    }
    sequences
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

/// Note transport cursor
///
/// Identifies a position in the note transport service's stored-note sequence.
///
/// The sequence is global across tags in one service database. Compare sequences only when their
/// nonces match.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct NoteTransportCursor(Option<(u64, u64)>);

impl NoteTransportCursor {
    /// Returns the cursor that starts from the first retained note.
    pub fn init() -> Self {
        Self(None)
    }

    /// Builds a cursor from the nonce and sequence returned by the transport service.
    pub fn from_parts(nonce: u64, sequence: u64) -> Self {
        Self(Some((nonce, sequence)))
    }

    /// Returns the nonce and sequence, or `None` for the initial cursor.
    pub fn parts(&self) -> Option<(u64, u64)> {
        self.0
    }
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

// RELAY OUTBOX
// ================================================================================================

/// A private note whose transport delivery has not yet succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RelayOutboxEntry {
    note: TransportNote,
    inclusion_proof: NoteInclusionProof,
}

impl RelayOutboxEntry {
    /// Sends the note and its inclusion proof through the transport.
    async fn relay(&self, api: &dyn NoteTransportClient) -> Result<(), NoteTransportError> {
        api.send_note_with_proof(self.note.clone(), self.inclusion_proof.clone()).await
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

impl Serializable for RelayOutboxEntry {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.note.write_into(target);
        self.inclusion_proof.write_into(target);
    }
}

impl Deserializable for RelayOutboxEntry {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let note = TransportNote::read_from(source)?;
        let inclusion_proof = NoteInclusionProof::read_from(source)?;
        Ok(Self { note, inclusion_proof })
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
        self.0.write_into(target);
    }
}

impl Deserializable for NoteTransportCursor {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        Ok(Self(Option::<(u64, u64)>::read_from(source)?))
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
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::NoteType;
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_standards::note::P2idNote;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;

    use super::*;
    use crate::rng::draw_word;

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

    #[test]
    fn relay_outbox_entry_round_trips() {
        let sender = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let target = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let faucet = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let mut rng = ChaCha20Rng::seed_from_u64(0);
        let note: Note = P2idNote::builder()
            .sender(sender)
            .target(target)
            .asset(FungibleAsset::new(faucet, 100).unwrap())
            .note_type(NoteType::Private)
            .serial_number(draw_word(&mut rng))
            .build()
            .unwrap()
            .into();

        let inclusion_proof =
            NoteInclusionProof::new(BlockNumber::from(7), 3, SparseMerklePath::default()).unwrap();
        let entry = RelayOutboxEntry {
            note: TransportNote::from(note),
            inclusion_proof,
        };

        assert_eq!(RelayOutboxEntry::read_from_bytes(&entry.to_bytes()).unwrap(), entry);
    }
}

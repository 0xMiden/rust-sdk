pub mod errors;
pub mod generated;
#[cfg(feature = "tonic")]
pub mod grpc;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use futures::Stream;
use miden_protocol::address::Address;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{Note, NoteDetails, NoteDetailsCommitment, NoteHeader, NoteId, NoteTag};
use miden_protocol::utils::serde::Serializable;
use miden_standards::note::{NoteFile, NoteSyncHint};
use miden_tx::auth::TransactionAuthenticator;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    SliceReader,
};

pub use self::errors::NoteTransportError;
use crate::{Client, ClientError};

pub const NOTE_TRANSPORT_TESTNET_ENDPOINT: &str = "https://transport.miden.io";
pub const NOTE_TRANSPORT_DEVNET_ENDPOINT: &str = "https://transport.devnet.miden.io";
pub const NOTE_TRANSPORT_CURSOR_STORE_SETTING: &str = "note_transport_cursor";

/// Settings key for the durable relay outbox: a serialized `Vec<NoteInfo>` of
/// private notes whose transport delivery has not yet succeeded.
/// `send_private_note` appends (replacing any entry with the same note id)
/// before relaying; [`Client::flush_relay_outbox`] drains entries that re-send
/// successfully. Reusing the settings k/v avoids a Store-trait schema change
/// while surviving process restarts.
pub const NOTE_TRANSPORT_OUTBOX_KEY: &str = "note_transport_outbox";

/// Client note transport methods.
impl<AUTH> Client<AUTH> {
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

    /// Send a note through the note transport network.
    ///
    /// The note will be end-to-end encrypted (unimplemented, currently plaintext)
    /// using the provided recipient's `address` details.
    /// The recipient will be able to retrieve this note through the note's [`NoteTag`].
    ///
    /// **Durability.** The relay payload is persisted to the outbox before the
    /// transport call. If the call fails or is interrupted, the entry stays in
    /// the outbox and is retried on the next [`Client::flush_relay_outbox`]
    /// (which [`Client::sync_note_transport`] runs), so a transient transport
    /// failure does not drop the note. The receiver dedupes by note id, so a
    /// re-send after a partial success is harmless.
    ///
    /// Prefer [`Client::send_private_note_with_block_hint`], which also relays a block hint so the
    /// recipient gets deterministic delivery instead of relying on its lookback heuristic.
    #[deprecated(
        since = "0.15.2",
        note = "use `Client::send_private_note_with_block_hint` to relay a block hint for deterministic delivery"
    )]
    pub async fn send_private_note(
        &mut self,
        note: Note,
        address: &Address,
    ) -> Result<(), ClientError> {
        self.relay_private_note(note, address, None).await
    }

    /// Send a note through the note transport network, relaying a block hint to the recipient.
    ///
    /// `block_hint` is the block from which the recipient should start scanning for the note's
    /// on-chain commitment, instead of relying on its lookback heuristic. Any block at or before
    /// the commitment is correct, and the chain tip at send time is a safe choice. A tighter value
    /// just means less for the recipient to scan.
    ///
    /// The same durability guarantees as [`Client::send_private_note`] apply: the hint is
    /// persisted with the relay payload, so a retried send preserves it.
    pub async fn send_private_note_with_block_hint(
        &mut self,
        note: Note,
        address: &Address,
        block_hint: BlockNumber,
    ) -> Result<(), ClientError> {
        self.relay_private_note(note, address, Some(block_hint)).await
    }

    /// Shared relay path for [`Client::send_private_note`] and
    /// [`Client::send_private_note_with_block_hint`]. `block_hint` is the optional block from which
    /// the recipient should start scanning for the note's commitment.
    async fn relay_private_note(
        &self,
        note: Note,
        _address: &Address,
        block_hint: Option<BlockNumber>,
    ) -> Result<(), ClientError> {
        let api = self.get_note_transport_api()?;

        let header = *note.header();
        let note_id = header.id();
        let details = NoteDetails::from(note);
        let details_bytes = details.to_bytes();
        // e2ee impl hint:
        // address.key().encrypt(details_bytes)

        // Persist the payload before the network call so a failed or
        // interrupted `send_note` leaves a recoverable record rather than
        // losing the only copy with the call frame. The hint travels with the
        // entry so a retried send relays the same value.
        let entry = NoteInfo {
            header,
            details_bytes: details_bytes.clone(),
            block_hint,
        };
        let mut outbox = self.load_relay_outbox().await?;
        // Replace any existing entry for this note id so the latest payload
        // wins when a still-pending note is re-sent.
        outbox.retain(|e| e.header.id() != note_id);
        outbox.push(entry);
        self.save_relay_outbox(outbox).await?;

        // Dispatch to the hint-carrying API only when a hint is present, otherwise use the plain
        // `send_note`. The transport exposes a separate method per scenario.
        match block_hint {
            Some(block_hint) => {
                api.send_note_with_block_hint(header, details_bytes, block_hint).await?;
            },
            None => {
                api.send_note(header, details_bytes).await?;
            },
        }

        // Relay succeeded — drop the entry. A failed store write here is
        // tolerable: the next flush re-sends and the receiver dedupes by note
        // id, so a stale entry never causes loss.
        let mut outbox = self.load_relay_outbox().await?;
        outbox.retain(|e| e.header.id() != note_id);
        self.save_relay_outbox(outbox).await?;

        Ok(())
    }

    /// Re-attempt every relay payload in the durable outbox. Each entry is a
    /// private note whose previous transport delivery failed. Successful
    /// re-sends are dropped; failures are kept for the next call. Every entry
    /// is attempted independently, so one persistently-failing note does not
    /// block the others.
    ///
    /// [`Client::sync_note_transport`] runs this automatically and ignores its
    /// error, so a relay failure can't block a sync. Callers driving retries
    /// themselves can invoke it directly and inspect the returned error.
    pub async fn flush_relay_outbox(&self) -> Result<(), ClientError> {
        let api = self.get_note_transport_api()?;

        let entries = self.load_relay_outbox().await?;
        if entries.is_empty() {
            return Ok(());
        }

        // Attempt every entry independently so a single persistently-failing
        // note can't block the rest. The outbox holds only the caller's own
        // failed sends, so it stays small and this is not a meaningful burst.
        let mut remaining = Vec::new();
        let mut last_err: Option<NoteTransportError> = None;

        for entry in entries {
            let relayed = match entry.block_hint {
                Some(block_hint) => {
                    api.send_note_with_block_hint(
                        entry.header,
                        entry.details_bytes.clone(),
                        block_hint,
                    )
                    .await
                },
                None => api.send_note(entry.header, entry.details_bytes.clone()).await,
            };
            match relayed {
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
    /// Returns an empty `Vec` if the outbox key is absent. On deserialization
    /// failure (schema mismatch or storage corruption) the entry is dropped and
    /// an empty `Vec` is returned — leaving unreadable bytes in place would
    /// block every subsequent relay because each sync would re-read them.
    async fn load_relay_outbox(&self) -> Result<Vec<NoteInfo>, ClientError> {
        let bytes = self
            .store
            .get_setting(String::from(NOTE_TRANSPORT_OUTBOX_KEY))
            .await
            .map_err(ClientError::StoreError)?;
        let Some(bytes) = bytes else {
            return Ok(Vec::new());
        };
        match Vec::<NoteInfo>::read_from_bytes(&bytes) {
            Ok(entries) => Ok(entries),
            Err(err) => {
                tracing::warn!(?err, "dropping unreadable relay outbox; resetting to empty");
                self.store
                    .remove_setting(String::from(NOTE_TRANSPORT_OUTBOX_KEY))
                    .await
                    .map_err(ClientError::StoreError)?;
                Ok(Vec::new())
            },
        }
    }

    /// Persist the relay outbox, removing the key entirely when empty so the
    /// settings table doesn't accumulate empty-vec blobs.
    async fn save_relay_outbox(&self, entries: Vec<NoteInfo>) -> Result<(), ClientError> {
        let key = String::from(NOTE_TRANSPORT_OUTBOX_KEY);
        if entries.is_empty() {
            return self.store.remove_setting(key).await.map_err(ClientError::StoreError);
        }
        let bytes = entries.to_bytes();
        self.store.set_setting(key, bytes).await.map_err(ClientError::StoreError)
    }
}

impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Fetch notes for tracked note tags.
    ///
    /// The client will query the configured note transport node for all tracked note tags.
    /// To list tracked tags please use [`Client::get_note_tags`]. To add a new note tag please use
    /// [`Client::add_note_tag`].
    /// Only notes directed at your addresses will be stored and readable given the use of
    /// end-to-end encryption (unimplemented).
    /// Fetched notes will be stored into the client's store.
    ///
    /// An internal pagination mechanism is employed to reduce the number of downloaded notes.
    /// To fetch the full history of private notes for the tracked tags, use
    /// [`Client::fetch_all_private_notes`].
    pub async fn fetch_private_notes(&mut self) -> Result<(), ClientError> {
        let note_tags: Vec<NoteTag> =
            self.store.get_unique_note_tags().await?.into_iter().collect();
        let cursor = self.store.get_note_transport_cursor().await?;

        let (_, new_cursor) = self.fetch_transport_notes(cursor, &note_tags).await?;
        self.store.update_note_transport_cursor(new_cursor).await?;

        Ok(())
    }

    /// Fetches all notes for tracked note tags, draining the server's paginated
    /// response by looping until the cursor stops advancing.
    ///
    /// Similar to [`Client::fetch_private_notes`] but ignores the stored
    /// pagination cursor and re-scans from the beginning. The server-side
    /// transport caps each response at a fixed batch size; this method issues
    /// repeated fetch calls until one returns the same cursor it was given
    /// (i.e. no new notes), so the documented "fetches all notes" semantics
    /// hold regardless of how large the backlog is. Prefer
    /// [`Client::fetch_private_notes`] for steady-state syncing to avoid
    /// re-downloading already-seen notes.
    pub async fn fetch_all_private_notes(&mut self) -> Result<(), ClientError> {
        // Safety cap on a misbehaving server. At 500 notes per batch, 1000
        // iterations covers 500k notes — well beyond any plausible retention
        // window — and bounds the worst-case wall-clock at ~50s at 50ms/req.
        // Hitting this signals a server bug, not an honest backlog.
        const MAX_ITERATIONS: usize = 1_000;

        let note_tags: Vec<NoteTag> =
            self.store.get_unique_note_tags().await?.into_iter().collect();
        // Snapshot the stored cursor up front so we can advance (never regress)
        // it after the drain. Without this guard, starting the drain at
        // `init()` and persisting per-batch would clobber a previously
        // advanced cursor with the small `rcursor` of the first batch.
        let stored_cursor = self.store.get_note_transport_cursor().await?;

        let mut cursor = NoteTransportCursor::init();
        for _ in 0..MAX_ITERATIONS {
            let (_, new_cursor) = self.fetch_transport_notes(cursor, &note_tags).await?;
            // Terminate on any lack of forward progress. A well-behaved server
            // returns `new_cursor == cursor` when there are no new notes (since
            // `rcursor = max(cursor, max_seq_returned)`); using `<=` here also
            // handles implementations that return an `init()` cursor on empty
            // batches (see the in-tree mock transport).
            if new_cursor <= cursor {
                let final_cursor = core::cmp::max(cursor, stored_cursor);
                self.store.update_note_transport_cursor(final_cursor).await?;
                return Ok(());
            }
            cursor = new_cursor;
        }

        Err(ClientError::NoteTransportError(NoteTransportError::PaginationDidNotTerminate(
            MAX_ITERATIONS,
        )))
    }

    /// Fetch one batch of notes from the note transport network for the provided tags.
    ///
    /// The server paginates; this method issues one RPC and returns the imported details
    /// commitments together with the new cursor. The returned cursor equals the input cursor when
    /// the batch was empty (i.e. no new notes). Callers that want to drain the full backlog should
    /// loop until `new_cursor == cursor` (see [`Client::fetch_all_private_notes`]). Callers that do
    /// steady-state polling (see [`Client::sync_state`] / [`Client::fetch_private_notes`]) should
    /// call this once per tick with the stored cursor.
    ///
    /// Downloaded notes are imported into the local store. Persistence of the returned cursor is
    /// left to the caller so that drain loops can guard against regression of an already-advanced
    /// stored cursor.
    pub(crate) async fn fetch_transport_notes(
        &mut self,
        cursor: NoteTransportCursor,
        tags: &[NoteTag],
    ) -> Result<(Vec<NoteId>, NoteTransportCursor), ClientError> {
        // Fallback lookback window, in blocks, used only for notes the transport delivered
        // without a sender-provided block hint. Scanning back from sync height handles
        // the race where a note is committed on-chain just before the NTL delivers its data.
        // Without it, check_expected_notes would scan from sync_height forward and miss the
        // already-committed note. A sender-provided hint is deterministic and always preferred.
        const NOTE_LOOKBACK_BLOCKS: u32 = 20;

        let mut notes = Vec::new();
        let (note_infos, rcursor) =
            self.get_note_transport_api()?.fetch_notes(tags, cursor).await?;
        for note_info in &note_infos {
            // e2ee impl hint:
            // for key in self.store.decryption_keys() try
            // key.decrypt(details_bytes_encrypted)
            let note = rejoin_note(&note_info.header, &note_info.details_bytes)?;
            notes.push((note, note_info.block_hint));
        }

        let sync_height = self.get_sync_height().await?;
        let fallback_after_block_num =
            BlockNumber::from(sync_height.as_u32().saturating_sub(NOTE_LOOKBACK_BLOCKS));

        let id_by_commitment: BTreeMap<NoteDetailsCommitment, NoteId> =
            notes.iter().map(|(note, _)| (note.details_commitment(), note.id())).collect();

        let mut note_requests = Vec::with_capacity(notes.len());
        for (note, block_hint) in notes {
            let tag = note.metadata().tag();
            // Prefer the sender-provided hint, falling back to the lookback window when absent.
            let after_block_num = block_hint.unwrap_or(fallback_after_block_num);
            let note_file = NoteFile::ExpectedNote {
                details: note.into(),
                sync_hint: NoteSyncHint::new(after_block_num, tag),
            };
            note_requests.push(note_file);
        }
        let imported_commitments = self.import_notes(&note_requests).await?;
        let imported_ids = imported_commitments
            .into_iter()
            .filter_map(|commitment| id_by_commitment.get(&commitment).copied())
            .collect();

        Ok((imported_ids, rcursor))
    }
}

/// Note transport cursor
///
/// Pagination integer used to reduce the number of fetched notes from the note transport network,
/// avoiding duplicate downloads.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Eq, Ord)]
pub struct NoteTransportCursor(u64);

/// Note Transport update
pub struct NoteTransportUpdate {
    /// Pagination cursor for next fetch
    pub cursor: NoteTransportCursor,
    /// Fetched notes
    pub notes: Vec<Note>,
}

impl NoteTransportCursor {
    pub fn new(value: u64) -> Self {
        Self(value)
    }

    pub fn init() -> Self {
        Self::new(0)
    }

    pub fn value(&self) -> u64 {
        self.0
    }
}

impl From<u64> for NoteTransportCursor {
    fn from(value: u64) -> Self {
        Self::new(value)
    }
}

/// The main transport client trait for sending and receiving encrypted notes
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait NoteTransportClient: Send + Sync {
    /// Send a note with optionally encrypted details
    async fn send_note(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
    ) -> Result<(), NoteTransportError>;

    /// Send a note, relaying a block hint for the recipient's commitment scan.
    ///
    /// `block_hint` is the block from which the recipient should start scanning for the
    /// note's commitment. The default implementation ignores it and delegates to
    /// [`NoteTransportClient::send_note`], so existing implementors keep compiling. Transports
    /// that can carry the hint (e.g. the gRPC client) override this.
    async fn send_note_with_block_hint(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        _block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_note(header, details).await
    }

    /// Fetch notes for given tags
    ///
    /// Downloads notes for given tags.
    /// Returns notes labelled after the provided cursor (pagination), and an updated cursor.
    async fn fetch_notes(
        &self,
        tag: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError>;

    /// Stream notes for a given tag
    async fn stream_notes(
        &self,
        tag: NoteTag,
        cursor: NoteTransportCursor,
    ) -> Result<Box<dyn NoteStream>, NoteTransportError>;
}

/// Stream trait for note streaming
pub trait NoteStream:
    Stream<Item = Result<Vec<NoteInfo>, NoteTransportError>> + Send + Unpin
{
}

/// Information about a note fetched from the note transport network
#[derive(Debug, Clone)]
pub struct NoteInfo {
    /// Note header
    pub header: NoteHeader,
    /// Note details, can be encrypted
    pub details_bytes: Vec<u8>,
    /// Sender-provided block hint: the block from which the recipient should start scanning for
    /// the note's on-chain commitment, instead of applying its default lookback window. `None`
    /// when the sender did not provide a hint.
    pub block_hint: Option<BlockNumber>,
}

impl NoteInfo {
    /// Build a [`NoteInfo`] without a block hint (`block_hint` is `None`).
    ///
    /// Use the [`NoteInfo::block_hint`] field directly to attach a hint.
    pub fn new(header: NoteHeader, details_bytes: Vec<u8>) -> Self {
        Self { header, details_bytes, block_hint: None }
    }
}

// SERIALIZATION
// ================================================================================================

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
        let value = u64::read_from(source)?;
        Ok(Self::new(value))
    }
}

fn rejoin_note(header: &NoteHeader, details_bytes: &[u8]) -> Result<Note, DeserializationError> {
    let mut reader = SliceReader::new(details_bytes);
    let details = NoteDetails::read_from(&mut reader)?;
    // The transport wire format only carries `NoteHeader` + serialized `NoteDetails`, not the
    // attachments collection. We rejoin with empty attachments; this matches the original note
    // only when it had no attachments in the first place.
    let partial_metadata = *header.metadata().partial_metadata();
    Ok(Note::new(
        details.assets().clone(),
        partial_metadata,
        details.recipient().clone(),
    ))
}

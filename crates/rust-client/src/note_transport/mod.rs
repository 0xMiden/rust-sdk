pub mod errors;
pub mod generated;
#[cfg(feature = "tonic")]
pub mod grpc;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::slice;

use futures::Stream;
use miden_protocol::address::Address;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::crypto::merkle::MerklePath;
use miden_protocol::crypto::merkle::mmr::PartialMmr;
use miden_protocol::note::{Note, NoteDetails, NoteDetailsCommitment, NoteHeader, NoteId, NoteTag};
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
use crate::note::ExpectedNoteImport;
use crate::store::InputNoteRecord;
use crate::sync::NoteTagSource;
use crate::{Client, ClientError};

pub const NOTE_TRANSPORT_TESTNET_ENDPOINT: &str = "https://transport.miden.io";
pub const NOTE_TRANSPORT_DEVNET_ENDPOINT: &str = "https://transport.devnet.miden.io";
pub const NOTE_TRANSPORT_CURSOR_STORE_SETTING: &str = "note_transport_cursor";

/// Settings key for the note-transport backfill bookkeeping: a serialized `Vec<NoteTag>` of the
/// `User`- and `Account`-source tags whose full history has already been fetched up to the global
/// cursor. [`Client::sync_note_transport`] diffs the currently tracked tags against this set to
/// find tags added after the cursor advanced, and backfills only those. Reusing the settings k/v
/// avoids a Store-trait schema change while surviving process restarts.
pub const NOTE_TRANSPORT_COVERED_TAGS_KEY: &str = "note_transport_covered_tags";

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
            self.store.remove_setting(key).await.map_err(ClientError::StoreError)?;
            return Ok(());
        }
        let bytes = entries.to_bytes();
        self.store.set_setting(key, bytes).await.map_err(ClientError::StoreError)
    }

    /// The set of tracked tags eligible for history backfill.
    ///
    /// Only `User`- and `Account`-source tags qualify: those are the tags a consumer explicitly
    /// started tracking (via [`Client::add_note_tag`], account import, or address creation) and may
    /// therefore have historical private notes sitting below the global cursor. `Note`-source tags
    /// are created by transport delivery and note import, so backfilling them would re-fetch tags
    /// the fetch path itself just registered; `Subscription` tags are excluded for the same reason.
    async fn backfill_candidate_tags(&self) -> Result<BTreeSet<NoteTag>, ClientError> {
        let tags = self
            .store
            .get_note_tags()
            .await?
            .into_iter()
            .filter(|record| {
                matches!(record.source, NoteTagSource::User | NoteTagSource::Account(_))
            })
            .map(|record| record.tag)
            .collect();
        Ok(tags)
    }

    /// Load the set of tags whose history has already been fetched up to the global cursor.
    ///
    /// Returns an empty set when the key is absent (e.g. a store that predates the feature). On a
    /// deserialization failure the entry is dropped and an empty set is returned: re-treating every
    /// tracked tag as new only triggers a one-off backfill, which dedupes, whereas leaving
    /// unreadable bytes in place would fail every subsequent sync.
    async fn load_covered_tags(&self) -> Result<BTreeSet<NoteTag>, ClientError> {
        let bytes = self
            .store
            .get_setting(String::from(NOTE_TRANSPORT_COVERED_TAGS_KEY))
            .await
            .map_err(ClientError::StoreError)?;
        let Some(bytes) = bytes else {
            return Ok(BTreeSet::new());
        };
        match BTreeSet::<NoteTag>::read_from_bytes(&bytes) {
            Ok(tags) => Ok(tags),
            Err(err) => {
                tracing::warn!(?err, "dropping unreadable covered-tags set; resetting to empty");
                self.store
                    .remove_setting(String::from(NOTE_TRANSPORT_COVERED_TAGS_KEY))
                    .await
                    .map_err(ClientError::StoreError)?;
                Ok(BTreeSet::new())
            },
        }
    }

    /// Persist the covered-tags set, removing the key entirely when empty so the settings table
    /// doesn't accumulate empty-vec blobs.
    async fn save_covered_tags(&self, tags: &BTreeSet<NoteTag>) -> Result<(), ClientError> {
        let key = String::from(NOTE_TRANSPORT_COVERED_TAGS_KEY);
        if tags.is_empty() {
            self.store.remove_setting(key).await.map_err(ClientError::StoreError)?;
            return Ok(());
        }
        self.store
            .set_setting(key, tags.to_bytes())
            .await
            .map_err(ClientError::StoreError)
    }
}

impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Per-sync cap on the number of newly tracked tags to backfill. Bounds the burst when many
    /// tags are registered at once (e.g. restoring many accounts or addresses). Deferred tags stay
    /// uncovered and are picked up on subsequent syncs.
    pub const MAX_BACKFILL_TAGS_PER_SYNC: usize = 64;

    /// Safety cap on the per-tag backfill drain. A well-behaved server eventually returns no
    /// forward cursor progress, ending the loop; this bound only guards against a server that
    /// advances the cursor indefinitely without ever returning an empty batch. It is far above any
    /// honest per-tag backlog, so reaching it signals a server bug rather than real history.
    const MAX_BACKFILL_ITERATIONS: usize = 1_000;

    /// Fetch notes for tracked note tags.
    ///
    /// The client will query the configured note transport node for all tracked note tags.
    /// To list tracked tags please use [`Client::get_note_tags`]. To add a new note tag please use
    /// [`Client::add_note_tag`].
    /// Only notes directed at your addresses will be stored and readable given the use of
    /// end-to-end encryption (unimplemented).
    /// Fetched notes will be stored into the client's store.
    ///
    /// An internal pagination mechanism is employed to reduce the number of downloaded notes: this
    /// fetches only notes past the stored cursor. Historical notes for a newly tracked tag are
    /// recovered automatically by [`Client::sync_note_transport`], which backfills each new tag.
    pub async fn fetch_private_notes(&mut self) -> Result<(), ClientError> {
        self.ensure_genesis_in_place().await?;

        let note_tags: Vec<NoteTag> =
            self.store.get_unique_note_tags().await?.into_iter().collect();
        let cursor = self.store.get_note_transport_cursor().await?;

        let mut id_by_commitment = BTreeMap::new();
        let (mut import, new_cursor) =
            self.fetch_transport_page(cursor, &note_tags, &mut id_by_commitment).await?;
        let blocks = self.fetch_note_block_proofs(slice::from_mut(&mut import)).await?;

        let mut partial_mmr = self.get_current_partial_mmr().await?;
        self.apply_blocks(blocks, &mut partial_mmr).await?;
        self.apply_expected_note_import(import).await?;
        self.cache_partial_mmr(partial_mmr).await?;

        self.store.update_note_transport_cursor(new_cursor).await?;

        Ok(())
    }

    /// Plans the backfill of historical private notes for tags added after the global cursor
    /// advanced.
    ///
    /// The global transport cursor is shared across all tracked tags and only moves forward, so a
    /// tag that starts being tracked late never sees its notes that already sit below the cursor.
    /// This diffs the tracked `User`/`Account` tags (see [`Self::backfill_candidate_tags`]) against
    /// the persisted covered set (see [`NOTE_TRANSPORT_COVERED_TAGS_KEY`]) and drains each newly
    /// tracked tag from the start, fetching only that tag's own history rather than re-scanning
    /// everything. Tags no longer tracked are dropped from the covered set so a later re-add
    /// backfills again instead of resuming from a stale mark. Imports dedupe, so the overlap with
    /// the steady-state stream is harmless.
    ///
    /// At most [`Self::MAX_BACKFILL_TAGS_PER_SYNC`] tags are backfilled per call; any remainder
    /// stays uncovered and is picked up on the next sync.
    ///
    /// Returns the pruned covered set, whether pruning changed it, and the tags to backfill.
    /// Reads only: persisting the covered set is left to the apply phase, which writes it after
    /// the imported notes so a crash re-backfills instead of skipping a tag whose notes were
    /// never written.
    async fn plan_backfill(&self) -> Result<(BTreeSet<NoteTag>, bool, Vec<NoteTag>), ClientError> {
        let candidates = self.backfill_candidate_tags().await?;
        let loaded = self.load_covered_tags().await?;

        // Drop tags no longer tracked. Keeping a removed tag marked covered would make a later
        // re-add skip its backlog, silently missing notes that arrived while it was untracked.
        let covered: BTreeSet<NoteTag> = loaded.intersection(&candidates).copied().collect();
        let pruned = covered.len() != loaded.len();

        let new_tags: Vec<NoteTag> = candidates
            .difference(&covered)
            .copied()
            .take(Self::MAX_BACKFILL_TAGS_PER_SYNC)
            .collect();

        Ok((covered, pruned, new_tags))
    }

    /// Drain a single tag's full history from the transport, paging until the cursor stops
    /// advancing. Uses a local cursor and never touches the global one, so it cannot regress
    /// steady-state progress. Returns one import per fetched page, none of them written.
    async fn backfill_tag(
        &self,
        tag: NoteTag,
        id_by_commitment: &mut BTreeMap<NoteDetailsCommitment, NoteId>,
    ) -> Result<Vec<ExpectedNoteImport>, ClientError> {
        let mut imports = Vec::new();
        let mut cursor = NoteTransportCursor::init();
        for _ in 0..Self::MAX_BACKFILL_ITERATIONS {
            let (import, new_cursor) =
                self.fetch_transport_page(cursor, &[tag], id_by_commitment).await?;
            imports.push(import);
            // Terminate on any lack of forward progress. A well-behaved server returns
            // `new_cursor == cursor` when there are no new notes for this tag (since
            // `rcursor = max(cursor, max_seq_returned)`); using `<=` also handles implementations
            // that return an `init()` cursor on empty batches (see the in-tree mock transport).
            if new_cursor <= cursor {
                return Ok(imports);
            }
            cursor = new_cursor;
        }

        Err(ClientError::NoteTransportError(NoteTransportError::PaginationDidNotTerminate(
            Self::MAX_BACKFILL_ITERATIONS,
        )))
    }

    /// Fetch one batch of notes from the note transport network for the provided tags and build
    /// the records they imply, without writing anything.
    ///
    /// The server paginates; this method issues one RPC and returns the import together with the
    /// new cursor. The returned cursor equals the input cursor when the batch was empty (i.e. no
    /// new notes). Callers that want to drain a tag's full backlog should loop until
    /// `new_cursor == cursor` (see [`Client::backfill_tag`]). Callers that do steady-state polling
    /// (see [`Client::sync_state`] / [`Client::fetch_private_notes`]) should call this once per
    /// tick with the stored cursor.
    ///
    /// Each downloaded note's id is recorded in `id_by_commitment` so the caller can resolve the
    /// written records back to note ids once the final record set is known. Persistence of the
    /// returned cursor is left to the caller so that drain loops can guard against regression of
    /// an already-advanced stored cursor.
    async fn fetch_transport_page(
        &self,
        cursor: NoteTransportCursor,
        tags: &[NoteTag],
        id_by_commitment: &mut BTreeMap<NoteDetailsCommitment, NoteId>,
    ) -> Result<(ExpectedNoteImport, NoteTransportCursor), ClientError> {
        // Fallback lookback window, in blocks, used only for notes the transport delivered
        // without a sender-provided block hint. Scanning back from sync height handles
        // the race where a note is committed on-chain just before the NTL delivers its data.
        // Without it, check_expected_notes would scan from sync_height forward and miss the
        // already-committed note. A sender-provided hint is deterministic and always preferred.
        const NOTE_LOOKBACK_BLOCKS: u32 = 20;

        let mut notes = Vec::new();
        // TODO: perhaps we should not need to map received IDs with details commitments, and
        // instead we may allow `InputNoteRecord` to optionally keep NoteIds. Then within
        // `import_note` we could match everything by ID and remove this map check
        let (note_infos, rcursor) =
            self.get_note_transport_api()?.fetch_notes(tags, cursor).await?;
        for note_info in &note_infos {
            // e2ee impl hint:
            // for key in self.store.decryption_keys() try
            // key.decrypt(details_bytes_encrypted)
            let note = rejoin_note(&note_info.header, &note_info.details_bytes)?;

            // The header carries the attachment-aware (on-chain) note id; the rejoined note has
            // empty attachments and would hash to a different id, so key off the header.
            id_by_commitment.insert(note.details_commitment(), note_info.header.id());
            notes.push((note, note_info.block_hint));
        }

        let sync_height = self.get_sync_height().await?;
        let fallback_after_block_num =
            BlockNumber::from(sync_height.as_u32().saturating_sub(NOTE_LOOKBACK_BLOCKS));

        let mut requests = Vec::with_capacity(notes.len());
        for (note, block_hint) in notes {
            let tag = note.metadata().tag();
            // Prefer the sender-provided hint, falling back to the lookback window when absent.
            let after_block_num = block_hint.unwrap_or(fallback_after_block_num);
            requests.push((NoteDetails::from(note), after_block_num, tag));
        }

        let import = self.fetch_expected_note_imports(&requests).await?;

        Ok((import, rcursor))
    }

    /// Fetches what the note transport sync is about to write, writing only the relay outbox.
    ///
    /// Runs the relay-outbox flush, the per-tag history backfill and the steady-state page, and
    /// returns the latter two as a [`NoteTransportSyncData`] for
    /// [`Client::apply_note_transport_updates`]. Takes `&self` so it can run concurrently with the
    /// chain sync's fetch phase.
    ///
    /// The outbox flush is the exception to this being a read-only phase: it persists its own
    /// remaining entries rather than handing them to the apply phase. That write touches only the
    /// outbox setting and is safe to redo, so it does not affect what a failure part way through
    /// leaves behind for the notes, tags and cursor.
    ///
    /// The block proofs of the notes reported as committed are *not* fetched here: they are a
    /// second network pass over this result (see [`Client::fetch_note_block_proofs`]), because the
    /// blocks to prove are only known once every page has been fetched.
    ///
    /// Returns empty data when note transport is not configured.
    pub(crate) async fn fetch_note_transport_updates(
        &self,
    ) -> Result<NoteTransportSyncData, ClientError> {
        let mut data = NoteTransportSyncData::default();
        if !self.is_note_transport_enabled() {
            return Ok(data);
        }

        // Drain any private notes whose previous relay attempt failed. A flush error is logged,
        // not propagated: a failing relay must not block the sync, and the entries stay durable
        // for the next attempt. This is the one write the fetch phase performs; it touches only
        // the outbox setting, which is independent of everything the apply phase writes.
        if let Err(err) = self.flush_relay_outbox().await {
            tracing::warn!(?err, "relay outbox flush failed during sync; entries retained");
        }

        // Recover historical private notes for any tag added after the global cursor advanced.
        // This drains each newly tracked tag from the start, fetching only that tag's own history.
        let (mut covered, pruned, new_tags) = self.plan_backfill().await?;
        let backfilled = !new_tags.is_empty();
        for tag in new_tags {
            data.imports.extend(self.backfill_tag(tag, &mut data.id_by_commitment).await?);
            covered.insert(tag);
        }
        if pruned || backfilled {
            data.covered_tags = Some(covered);
        }

        let cursor = self.store.get_note_transport_cursor().await?;
        let note_tags: Vec<NoteTag> =
            self.store.get_unique_note_tags().await?.into_iter().collect();
        let (import, new_cursor) = self
            .fetch_transport_page(cursor, &note_tags, &mut data.id_by_commitment)
            .await?;
        data.imports.push(import);
        data.cursor = Some(new_cursor);

        Ok(data)
    }

    /// Writes everything [`Client::fetch_note_transport_updates`] fetched, returning the ids of
    /// the imported notes.
    ///
    /// The notes are written before the covered-tag set and the cursor, so a crash between them
    /// re-fetches instead of skipping notes that were never written. `partial_mmr` is loaded and
    /// cached by the caller, so one MMR can be shared with the chain sync's apply phase.
    ///
    /// The relay outbox is not written here: [`Client::flush_relay_outbox`] persists it itself,
    /// during the fetch phase.
    ///
    /// [`Client::fetch_note_block_proofs`] must have run on `data` first, otherwise the records
    /// waiting on a block header are still pending and the import panics.
    pub(crate) async fn apply_note_transport_updates(
        &mut self,
        data: NoteTransportSyncData,
        partial_mmr: &mut PartialMmr,
    ) -> Result<Vec<NoteId>, ClientError> {
        let NoteTransportSyncData {
            covered_tags,
            imports,
            blocks,
            id_by_commitment,
            cursor,
        } = data;

        self.apply_blocks(blocks, partial_mmr).await?;

        let mut imported_ids = Vec::new();
        for import in imports {
            let written = self.apply_expected_note_import(import).await?;
            imported_ids.extend(
                written
                    .into_iter()
                    .filter_map(|commitment| id_by_commitment.get(&commitment).copied()),
            );
        }

        if let Some(covered_tags) = covered_tags {
            self.save_covered_tags(&covered_tags).await?;
        }

        if let Some(cursor) = cursor {
            self.store.update_note_transport_cursor(cursor).await?;
        }

        imported_ids.sort_unstable();
        imported_ids.dedup();

        Ok(imported_ids)
    }
}

// NOTE TRANSPORT SYNC DATA
// ================================================================================================

/// Everything the note transport sync is about to write, with nothing written yet.
///
/// Built by [`Client::fetch_note_transport_updates`], completed by
/// [`Client::fetch_note_block_proofs`] and written by
/// [`Client::apply_note_transport_updates`].
#[derive(Default)]
pub(crate) struct NoteTransportSyncData {
    /// Covered-tag set to persist, `None` when it did not change.
    covered_tags: Option<BTreeSet<NoteTag>>,
    /// One entry per fetched page, in fetch order.
    pub(crate) imports: Vec<ExpectedNoteImport>,
    /// Headers of the blocks that committed the fetched notes, with the MMR proof paths the node
    /// returned. Filled by [`Client::fetch_note_block_proofs`].
    pub(crate) blocks: BTreeMap<BlockNumber, (BlockHeader, MerklePath)>,
    /// Note ids by details commitment, taken from the note headers the transport returned. Used
    /// to resolve the written records back to ids.
    id_by_commitment: BTreeMap<NoteDetailsCommitment, NoteId>,
    /// New global cursor, from the steady-state page. `None` when no page was fetched.
    cursor: Option<NoteTransportCursor>,
}

impl NoteTransportSyncData {
    /// The records this sync is about to write.
    ///
    /// Used to extend the chain sync's nullifier check to the notes the transport just delivered,
    /// which are not in the store yet.
    pub(crate) fn input_note_records(&self) -> impl Iterator<Item = &InputNoteRecord> {
        self.imports.iter().flat_map(ExpectedNoteImport::input_note_records)
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

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::pin::Pin;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::task::{Context, Poll};

use chrono::Utc;
use futures::Stream;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteTag};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};
use miden_tx::utils::sync::RwLock;

use crate::note_transport::{
    NoteInfo,
    NoteStream,
    NoteTransportClient,
    NoteTransportCursor,
    NoteTransportError,
};

/// Mock Note Transport Node
///
/// Simulates the functionality of the note transport node.
#[derive(Clone)]
pub struct MockNoteTransportNode {
    notes: BTreeMap<NoteTag, Vec<(NoteInfo, NoteTransportCursor)>>,
    /// Optional per-response batch cap; if `Some(n)`, `get_notes` returns at
    /// most `n` entries (total, across all tags) in one call. Used to exercise
    /// client-side pagination drain loops. `None` = unbounded (legacy behavior).
    max_batch: Option<usize>,
}

impl MockNoteTransportNode {
    pub fn new() -> Self {
        Self {
            notes: BTreeMap::default(),
            max_batch: None,
        }
    }

    /// Build a mock that caps each `get_notes` response at `max_batch` entries.
    pub fn with_max_batch(max_batch: usize) -> Self {
        Self {
            notes: BTreeMap::default(),
            max_batch: Some(max_batch),
        }
    }

    pub fn add_note(&mut self, header: NoteHeader, details_bytes: Vec<u8>) {
        self.add_note_after(header, details_bytes, None);
    }

    /// Seed a note carrying a sender-provided commitment block floor, mirroring a relay sent
    /// via [`Client::send_private_note_with_block_hint`](crate::Client::send_private_note_with_block_hint).
    pub fn add_note_after(
        &mut self,
        header: NoteHeader,
        details_bytes: Vec<u8>,
        block_hint: Option<BlockNumber>,
    ) {
        let tag = header.metadata().tag();
        let info = NoteInfo { header, details_bytes, block_hint };
        let cursor = u64::try_from(Utc::now().timestamp_micros()).unwrap();
        self.notes.entry(tag).or_default().push((info, cursor.into()));
    }

    pub fn get_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> (Vec<NoteInfo>, NoteTransportCursor) {
        // Start `rcursor` at the input — matches the real server's contract
        // (`rcursor = max(cursor, max_seq_returned)`), so an empty batch
        // returns the caller's own cursor rather than `init()`.
        let mut collected: Vec<(NoteInfo, NoteTransportCursor)> = vec![];
        for tag in tags {
            // Assumes stored notes are ordered by cursor
            let tnotes = self
                .notes
                .get(tag)
                .map(|pg_notes| {
                    // Find first element after cursor
                    if let Some(pos) = pg_notes.iter().position(|(_, tcursor)| *tcursor > cursor) {
                        &pg_notes[pos..]
                    } else {
                        &[]
                    }
                })
                .map(Vec::from)
                .unwrap_or_default();
            collected.extend(tnotes);
        }

        // Deterministic ordering across tags: sort by cursor ascending so the
        // client sees notes in per-cursor order regardless of tag iteration
        // order, matching the real server's `ORDER BY seq ASC`.
        collected.sort_by_key(|(_, c)| *c);

        // Apply the batch cap, if configured.
        if let Some(max) = self.max_batch {
            collected.truncate(max);
        }

        let rcursor = collected.iter().map(|(_, c)| *c).max().unwrap_or(cursor);
        let notes = collected.into_iter().map(|(n, _)| n).collect();
        (notes, rcursor)
    }
}

impl Default for MockNoteTransportNode {
    fn default() -> Self {
        Self::new()
    }
}

/// Mock Note Transport API
///
/// Simulates communications with the note transport node.
#[derive(Clone, Default)]
pub struct MockNoteTransportApi {
    pub mock_node: Arc<RwLock<MockNoteTransportNode>>,
}

impl MockNoteTransportApi {
    pub fn new(mock_node: Arc<RwLock<MockNoteTransportNode>>) -> Self {
        Self { mock_node }
    }
}

impl MockNoteTransportApi {
    pub fn send_note(&self, header: NoteHeader, details_bytes: Vec<u8>) {
        self.mock_node.write().add_note(header, details_bytes);
    }

    pub fn send_note_with_block_hint(
        &self,
        header: NoteHeader,
        details_bytes: Vec<u8>,
        block_hint: BlockNumber,
    ) {
        self.mock_node.write().add_note_after(header, details_bytes, Some(block_hint));
    }

    pub fn fetch_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> (Vec<NoteInfo>, NoteTransportCursor) {
        self.mock_node.read().get_notes(tags, cursor)
    }
}

pub struct DummyNoteStream {}
impl Stream for DummyNoteStream {
    type Item = Result<Vec<NoteInfo>, NoteTransportError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}
impl NoteStream for DummyNoteStream {}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl NoteTransportClient for MockNoteTransportApi {
    async fn send_note(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
    ) -> Result<(), NoteTransportError> {
        self.send_note(header, details);
        Ok(())
    }

    async fn send_note_with_block_hint(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_note_with_block_hint(header, details, block_hint);
        Ok(())
    }

    async fn fetch_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError> {
        Ok(self.fetch_notes(tags, cursor))
    }

    async fn stream_notes(
        &self,
        _tag: NoteTag,
        _cursor: NoteTransportCursor,
    ) -> Result<Box<dyn NoteStream>, NoteTransportError> {
        Ok(Box::new(DummyNoteStream {}))
    }
}

// FAULTY NOTE TRANSPORT API
// ================================================================================================

/// Test-only [`NoteTransportClient`] decorator that injects controlled failures
/// into `send_note` calls.
///
/// Reproduces the failure mode where the NTL is reachable but rejects (or
/// silently drops) a relay attempt, exercising the durable outbox in
/// [`Client::send_private_note`](crate::Client::send_private_note): without
/// retry/persistence a failed relay would leave the recipient unable to
/// discover the note.
///
/// The decorator counts attempts (`send_attempts`) and lets a test specify how
/// many of the next `send_note` calls should fail (`fail_next`); successful
/// calls delegate to an inner [`MockNoteTransportApi`]. `fetch_notes` and
/// `stream_notes` always delegate to the inner mock.
pub struct FaultyNoteTransportApi {
    inner: MockNoteTransportApi,
    fail_next: AtomicUsize,
    send_attempts: AtomicUsize,
}

impl FaultyNoteTransportApi {
    /// Create a faulty transport that fails the next `fail_next` `send_note`
    /// calls before delegating to the inner mock.
    pub fn new(mock_node: Arc<RwLock<MockNoteTransportNode>>, fail_next: usize) -> Self {
        Self {
            inner: MockNoteTransportApi::new(mock_node),
            fail_next: AtomicUsize::new(fail_next),
            send_attempts: AtomicUsize::new(0),
        }
    }

    /// Reset the fail-counter to `n`; subsequent `send_note` calls fail until
    /// the counter reaches zero.
    pub fn fail_next_n(&self, n: usize) {
        self.fail_next.store(n, Ordering::SeqCst);
    }

    /// Total `send_note` calls observed (success + failure).
    pub fn send_attempts(&self) -> usize {
        self.send_attempts.load(Ordering::SeqCst)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl NoteTransportClient for FaultyNoteTransportApi {
    async fn send_note(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
    ) -> Result<(), NoteTransportError> {
        self.send_attempts.fetch_add(1, Ordering::SeqCst);
        let should_fail = self
            .fail_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if should_fail {
            return Err(NoteTransportError::Network(
                "FaultyNoteTransportApi: simulated send_note failure".to_string(),
            ));
        }
        self.inner.send_note(header, details);
        Ok(())
    }

    async fn send_note_with_block_hint(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_attempts.fetch_add(1, Ordering::SeqCst);
        let should_fail = self
            .fail_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if should_fail {
            return Err(NoteTransportError::Network(
                "FaultyNoteTransportApi: simulated send_note failure".to_string(),
            ));
        }
        self.inner.send_note_with_block_hint(header, details, block_hint);
        Ok(())
    }

    async fn fetch_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError> {
        Ok(self.inner.fetch_notes(tags, cursor))
    }

    async fn stream_notes(
        &self,
        _tag: NoteTag,
        _cursor: NoteTransportCursor,
    ) -> Result<Box<dyn NoteStream>, NoteTransportError> {
        Ok(Box::new(DummyNoteStream {}))
    }
}

// SERIALIZATION
// ================================================================================================

impl Serializable for MockNoteTransportNode {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.notes.write_into(target);
    }
}

impl Deserializable for MockNoteTransportNode {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let notes = BTreeMap::<NoteTag, Vec<(NoteInfo, NoteTransportCursor)>>::read_from(source)?;

        Ok(Self { notes, max_batch: None })
    }
}

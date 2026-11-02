use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

pub use miden_client_core::testing::note_transport::MockNoteTransportNode;
use miden_protocol::note::{NoteInclusionProof, NoteTag};
use miden_tx::utils::serde::Serializable;
use miden_tx::utils::sync::RwLock;

use crate::note_transport::{
    NoteTransportClient,
    NoteTransportCursor,
    NoteTransportError,
    NoteTransportPage,
    TransportNote,
};

/// Mock Note Transport API
///
/// Simulates communications with the note transport node.
#[derive(Clone, Default)]
pub struct MockNoteTransportApi {
    pub mock_node: Arc<RwLock<MockNoteTransportNode>>,
    fetch_tag_counts: Arc<RwLock<Vec<usize>>>,
    fetch_bounds: Arc<RwLock<Vec<Option<NoteTransportCursor>>>>,
}

impl MockNoteTransportApi {
    pub fn new(mock_node: Arc<RwLock<MockNoteTransportNode>>) -> Self {
        Self {
            mock_node,
            fetch_tag_counts: Arc::default(),
            fetch_bounds: Arc::default(),
        }
    }

    /// Returns the number of tags in each fetch request.
    pub fn fetch_tag_counts(&self) -> Vec<usize> {
        self.fetch_tag_counts.read().clone()
    }

    /// Returns the `sync_to` bound of each fetch request.
    pub fn fetch_bounds(&self) -> Vec<Option<NoteTransportCursor>> {
        self.fetch_bounds.read().clone()
    }
}

impl MockNoteTransportApi {
    pub fn send_note_with_proof(&self, note: TransportNote, inclusion_proof: &NoteInclusionProof) {
        let (header, details) = note.into_parts();
        let details_bytes = details.to_bytes();
        self.mock_node
            .write()
            .add_note_with_proof(header, details_bytes, inclusion_proof);
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl NoteTransportClient for MockNoteTransportApi {
    async fn send_note_with_proof(
        &self,
        note: TransportNote,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), NoteTransportError> {
        self.send_note_with_proof(note, &inclusion_proof);
        Ok(())
    }

    async fn fetch_notes_page(
        &self,
        tags: &[NoteTag],
        cursor: Option<NoteTransportCursor>,
        sync_to: Option<NoteTransportCursor>,
    ) -> Result<NoteTransportPage, NoteTransportError> {
        self.fetch_tag_counts.write().push(tags.len());
        self.fetch_bounds.write().push(sync_to);
        let node = self.mock_node.read();
        let sync_to = sync_to.unwrap_or_else(|| node.head());
        let (notes, cursor) = node.notes_up_to(tags, cursor, sync_to);
        let has_more = !node.notes_up_to(tags, Some(cursor), sync_to).0.is_empty();
        Ok(NoteTransportPage { notes, cursor, has_more, sync_to })
    }
}

// FAULTY NOTE TRANSPORT API
// ================================================================================================

/// Test-only [`NoteTransportClient`] decorator that injects controlled failures into
/// `send_note_with_proof` calls.
///
/// Reproduces the failure mode where the NTL is reachable but rejects a send. Tests use it to check
/// how [`Client::send_private_note_with_proof`](crate::Client::send_private_note_with_proof)
/// reports the failure and how a later send by the caller delivers the note.
///
/// The decorator counts attempts (`send_attempts`) and lets a test specify how many of the next
/// `send_note_with_proof` calls should fail (`fail_next`); successful calls delegate to an inner
/// [`MockNoteTransportApi`]. `fetch_notes_page` failures can be injected separately via
/// [`FaultyNoteTransportApi::fail_next_n_fetches`].
pub struct FaultyNoteTransportApi {
    inner: MockNoteTransportApi,
    fail_next: AtomicUsize,
    send_attempts: AtomicUsize,
    fail_next_fetches: AtomicUsize,
    fail_on_fetch: AtomicUsize,
    fetch_attempts: AtomicUsize,
}

impl FaultyNoteTransportApi {
    /// Create a faulty transport that fails the next `fail_next` `send_note_with_proof` calls
    /// before delegating to the inner mock.
    pub fn new(mock_node: Arc<RwLock<MockNoteTransportNode>>, fail_next: usize) -> Self {
        Self {
            inner: MockNoteTransportApi::new(mock_node),
            fail_next: AtomicUsize::new(fail_next),
            send_attempts: AtomicUsize::new(0),
            fail_next_fetches: AtomicUsize::new(0),
            fail_on_fetch: AtomicUsize::new(0),
            fetch_attempts: AtomicUsize::new(0),
        }
    }

    /// Total `send_note_with_proof` calls observed (success + failure).
    pub fn send_attempts(&self) -> usize {
        self.send_attempts.load(Ordering::SeqCst)
    }

    /// Fail the next `n` `fetch_notes_page` calls before delegating to the inner mock again.
    pub fn fail_next_n_fetches(&self, n: usize) {
        self.fail_next_fetches.store(n, Ordering::SeqCst);
    }

    /// Fails one fetch attempt. Attempt numbers start at one.
    pub fn fail_on_fetch_attempt(&self, attempt: usize) {
        self.fail_on_fetch.store(attempt, Ordering::SeqCst);
    }

    /// Total `fetch_notes_page` calls observed (success + failure).
    pub fn fetch_attempts(&self) -> usize {
        self.fetch_attempts.load(Ordering::SeqCst)
    }

    /// Records a send attempt and returns whether it must fail.
    fn take_send_failure(&self) -> Option<NoteTransportError> {
        self.send_attempts.fetch_add(1, Ordering::SeqCst);
        self.fail_next
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
            .then(|| {
                NoteTransportError::Network(
                    "FaultyNoteTransportApi: simulated send_note_with_proof failure".to_string(),
                )
            })
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl NoteTransportClient for FaultyNoteTransportApi {
    async fn send_note_with_proof(
        &self,
        note: TransportNote,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), NoteTransportError> {
        if let Some(error) = self.take_send_failure() {
            return Err(error);
        }
        self.inner.send_note_with_proof(note, &inclusion_proof);
        Ok(())
    }

    async fn fetch_notes_page(
        &self,
        tags: &[NoteTag],
        cursor: Option<NoteTransportCursor>,
        sync_to: Option<NoteTransportCursor>,
    ) -> Result<NoteTransportPage, NoteTransportError> {
        let attempt = self.fetch_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        let should_fail = self
            .fail_next_fetches
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
        if should_fail || attempt == self.fail_on_fetch.load(Ordering::SeqCst) {
            return Err(NoteTransportError::Network(
                "FaultyNoteTransportApi: simulated fetch_notes_page failure".to_string(),
            ));
        }
        self.inner.fetch_notes_page(tags, cursor, sync_to).await
    }
}

//! Side-effect-only observer trait for per-note arrivals during sync.

use alloc::boxed::Box;

use async_trait::async_trait;
use miden_protocol::note::NoteAttachments;

use crate::ClientError;
use crate::rpc::domain::note::CommittedNote;
use crate::sync::StateSyncUpdate;

/// Per-note + post-sync side-channel into [`crate::sync::StateSync`].
/// Attach via `StateSync::with_note_observer(...)`. Multiple observers
/// run independently; errors are logged, never abort sync.
#[async_trait(?Send)]
pub trait NoteObserver {
    /// Identifier surfaced on `tracing::warn!` events for this observer.
    fn name(&self) -> &'static str;

    /// Per-note hook. Runs before the screener verdict. `attachments` is the note's resolved
    /// attachment content for this sync window (`None` if absent).
    ///
    /// Returns `true` to mark the enclosing block as relevant even if the screener discards it,
    /// so sync persists its header.
    async fn observe(
        &self,
        committed_note: &CommittedNote,
        attachments: Option<&NoteAttachments>,
    ) -> Result<bool, ClientError>;

    /// Post-sync hook, invoked once after the sync window closes.
    /// Default impl is a no-op for observers that only need `observe()`.
    async fn apply(&self, _sync_update: &StateSyncUpdate) -> Result<(), ClientError> {
        Ok(())
    }
}

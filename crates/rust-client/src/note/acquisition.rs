use alloc::boxed::Box;

use super::NoteFile;

/// How the client received the candidates for note acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteAcquisitionSource {
    /// An explicit note file import.
    Import,
    /// A delivery from the Note Transport Layer.
    NoteTransport,
}

/// The host did not retain the note candidates.
#[derive(Debug, thiserror::Error)]
#[error("the host did not acknowledge note retention")]
pub struct NoteAcquisitionError;

/// Confirms that the host retained incoming note candidates outside the client store.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait NoteAcquisitionGate: Send + Sync {
    /// Acknowledges retention before the client imports the candidates.
    ///
    /// Each call receives at most 32 files. The client can deliver the same candidate again when a
    /// later operation fails. The host must tolerate repeated delivery. Do not call back into the
    /// client from this method.
    ///
    /// Retention does not verify that the notes are valid or committed.
    async fn retain(
        &self,
        source: NoteAcquisitionSource,
        notes: &[NoteFile],
    ) -> Result<(), NoteAcquisitionError>;
}

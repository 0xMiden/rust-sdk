use alloc::boxed::Box;
use alloc::string::String;
use core::error::Error;

use miden_objects::ConversionError;
use miden_protocol::note::{NoteDetailsCommitment, NoteTag};
use miden_protocol::utils::serde::DeserializationError;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NoteTransportError {
    #[error(
        "note transport is disabled; enable it in the client configuration to send or receive notes via P2P"
    )]
    Disabled,
    #[error("connection error: {0}")]
    Connection(#[source] Box<dyn Error + Send + Sync + 'static>),
    #[error("deserialization error: {0}")]
    Deserialization(#[from] DeserializationError),
    #[error(
        "note details commitment {} does not match the header commitment {}",
        details.to_hex(),
        header.to_hex()
    )]
    NoteDetailsMismatch {
        header: NoteDetailsCommitment,
        details: NoteDetailsCommitment,
    },
    #[error("note transport returned an invalid note: {0}")]
    InvalidFetchedNote(#[source] ConversionError),
    #[error("note transport returned a note with tag {0}, which the client did not request")]
    UnrequestedTag(NoteTag),
    #[error("note transport network error: {0}")]
    Network(String),
    #[error(
        "note transport tag backfill did not converge after {0} iterations: the server cursor \
         keeps advancing but never returns an empty batch"
    )]
    PaginationDidNotTerminate(usize),
}

use alloc::boxed::Box;
use alloc::string::String;
use core::error::Error;

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
    #[error("note transport network error: {0}")]
    Network(String),
    #[error(
        "note transport tag backfill did not converge after {0} iterations: the server cursor \
         keeps advancing but never returns an empty batch"
    )]
    PaginationDidNotTerminate(usize),
}

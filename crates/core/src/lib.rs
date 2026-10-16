//! Core traits and types shared by the miden-client crates.

#![no_std]

#[macro_use]
extern crate alloc;

pub mod errors;
pub mod note_transport;
pub mod rpc;
pub mod store;
pub mod sync;
pub mod transaction;

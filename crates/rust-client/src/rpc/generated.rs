#[cfg(feature = "std")]
#[rustfmt::skip]
#[allow(dead_code)]
mod std_gen {
    include!(concat!(env!("OUT_DIR"), "/rpc_std.rs"));
}
#[cfg(feature = "std")]
pub use std_gen::*;

#[cfg(not(feature = "std"))]
#[rustfmt::skip]
#[allow(dead_code)]
mod nostd_gen {
    include!(concat!(env!("OUT_DIR"), "/rpc_nostd.rs"));
}
// CANONICAL OBJECT MESSAGES
// ================================================================================================

// The node services import the canonical object schemas, and the build script maps those packages
// onto the types that `miden-objects` generates. Re-export them here so that the whole protobuf
// surface the client talks to is reachable through one module.
pub use miden_objects::proto::{account, note, primitives, transaction};
#[cfg(not(feature = "std"))]
pub use nostd_gen::*;

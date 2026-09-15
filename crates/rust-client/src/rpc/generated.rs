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
// `build.rs` resolves the object schemas the node's descriptors import to `miden-objects`, so they
// are not generated here. Re-exporting them keeps every `proto::<package>::` path valid. The list
// mirrors `miden_objects::EXTERN_PATHS` even where the client uses only part of it.
#[allow(unused_imports)]
pub use miden_objects::proto::{
    account,
    asset,
    blockchain,
    note,
    primitives,
    protocol_config,
    transaction,
};
#[cfg(not(feature = "std"))]
pub use nostd_gen::*;

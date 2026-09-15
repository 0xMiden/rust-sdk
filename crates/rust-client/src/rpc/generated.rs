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
// These packages are not generated here. `build.rs` points prost at `miden-objects` for them, and
// this re-export keeps every `proto::<package>::` path valid. The list mirrors
// `miden_objects::EXTERN_PATHS`, which is why it carries packages no call site names yet.
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

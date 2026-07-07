#[cfg(feature = "std")]
#[rustfmt::skip]
#[allow(dead_code)]
mod std_gen {
    include!(concat!(env!("OUT_DIR"), "/remote_prover_std.rs"));
}
#[cfg(feature = "std")]
pub use std_gen::remote_prover::*;

#[cfg(not(feature = "std"))]
#[rustfmt::skip]
#[allow(dead_code)]
mod nostd_gen {
    include!(concat!(env!("OUT_DIR"), "/remote_prover_nostd.rs"));
}
#[cfg(not(feature = "std"))]
pub use nostd_gen::remote_prover::*;

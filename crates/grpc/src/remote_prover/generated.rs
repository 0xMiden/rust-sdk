#[rustfmt::skip]
#[allow(dead_code)]
mod inner {
    #[cfg(feature = "std")]
    include!(concat!(env!("OUT_DIR"), "/remote_prover_std.rs"));

    #[cfg(not(feature = "std"))]
    include!(concat!(env!("OUT_DIR"), "/remote_prover_nostd.rs"));
}
pub use inner::miden::remote_prover::v1::*;

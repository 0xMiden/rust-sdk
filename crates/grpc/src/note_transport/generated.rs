#[rustfmt::skip]
#[allow(dead_code)]
mod inner {
    #[cfg(feature = "std")]
    include!(concat!(env!("OUT_DIR"), "/note_transport_std.rs"));

    #[cfg(not(feature = "std"))]
    include!(concat!(env!("OUT_DIR"), "/note_transport_nostd.rs"));
}
pub use inner::miden::note_transport::v1::*;

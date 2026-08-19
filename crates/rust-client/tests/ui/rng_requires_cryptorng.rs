//! Do not delete. `cargo shear` reports this file as unlinked because trybuild loads it as
//! data rather than as a cargo target; the test that drives it is `builder::tests::ui`.
//!
//! `ClientBuilder::rng` must reject a generator that is not a `CryptoRng`, even when it
//! implements `FeltRng`. See `Client::secure_rng` for why the bound is there.

use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_protocol::crypto::rand::FeltRng;
use miden_protocol::{Felt, Word};

/// A fully predictable generator. It implements `TryRng` (and so `Rng`) and `FeltRng`, but
/// deliberately not `TryCryptoRng`.
struct FixedRng(u64);

impl rand::TryRng for FixedRng {
    type Error = core::convert::Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(self.0 as u32)
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(self.0)
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), Self::Error> {
        dest.fill(self.0 as u8);
        Ok(())
    }
}

impl FeltRng for FixedRng {
    fn draw_element(&mut self) -> Felt {
        Felt::new_unchecked(self.0)
    }

    fn draw_word(&mut self) -> Word {
        Word::new([self.draw_element(); 4])
    }
}

fn main() {
    let _ = ClientBuilder::<FilesystemKeyStore>::new().rng(Box::new(FixedRng(7)));
}

use crate::rpc::encryption::{SUPPORTED_SCHEME, SealedTransactionInputs};
use crate::rpc::generated as proto;
use crate::rpc::generated::submission::IesScheme;

// The domain constant must name the scheme that the wire enum defines.
const _: () = assert!(SUPPORTED_SCHEME == IesScheme::X25519Xchacha20Poly1305 as u32);

impl From<SealedTransactionInputs> for proto::submission::SealedTransactionInputs {
    fn from(sealed: SealedTransactionInputs) -> Self {
        let (key_id, ciphertext) = sealed.into_parts();
        Self { key_id, ciphertext }
    }
}

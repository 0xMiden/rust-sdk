//! Adapts the transaction encryption types to the node's protobuf.

use miden_client_core::rpc::encryption::SealedTransactionInputs;

use crate::generated as proto;

impl From<SealedTransactionInputs> for proto::submission::SealedTransactionInputs {
    fn from(sealed: SealedTransactionInputs) -> Self {
        Self {
            key_id: sealed.key_id().to_vec(),
            ciphertext: sealed.ciphertext().to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use miden_client_core::rpc::encryption::SUPPORTED_SCHEME;

    use super::*;

    #[test]
    fn supported_scheme_matches_the_node_protobuf() {
        assert_eq!(SUPPORTED_SCHEME, proto::submission::IesScheme::X25519Xchacha20Poly1305 as u32);
    }
}

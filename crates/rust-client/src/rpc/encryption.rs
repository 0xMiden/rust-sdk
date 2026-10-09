//! Adapts the transaction encryption types to the node's protobuf and RPC errors.

pub use miden_client_core::rpc::encryption::*;

use super::{RpcError, generated as proto};

impl From<SealedTransactionInputs> for proto::submission::SealedTransactionInputs {
    fn from(sealed: SealedTransactionInputs) -> Self {
        Self {
            key_id: sealed.key_id().to_vec(),
            ciphertext: sealed.ciphertext().to_vec(),
        }
    }
}

impl From<TransactionEncryptionError> for RpcError {
    fn from(err: TransactionEncryptionError) -> Self {
        match err {
            TransactionEncryptionError::KeyRejected(reason) => {
                RpcError::TransactionEncryptionKeyRejected(reason)
            },
            TransactionEncryptionError::SealingFailed(reason) => {
                RpcError::TransactionInputsSealingFailed(reason)
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_scheme_matches_the_node_protobuf() {
        assert_eq!(SUPPORTED_SCHEME, proto::submission::IesScheme::X25519Xchacha20Poly1305 as u32);
    }
}

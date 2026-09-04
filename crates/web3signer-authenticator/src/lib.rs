use miden_client::{
    AuthenticationError,
    auth::{PublicKeyCommitment, Signature, SigningInputs, TransactionAuthenticator},
};
use miden_processor::FutureMaybeSend;
use miden_protocol::account::auth::PublicKey;
use std::sync::Arc;

pub struct Web3SignerAuthenticator {}

impl TransactionAuthenticator for Web3SignerAuthenticator {
    fn get_signature(
        &self,
        _pub_key_commitment: PublicKeyCommitment,
        _signing_inputs: &SigningInputs,
    ) -> impl FutureMaybeSend<Result<Signature, AuthenticationError>> {
        async { todo!() }
    }

    /// Retrieves a public key for a specific public key commitment.
    fn get_public_key(
        &self,
        _pub_key_commitment: PublicKeyCommitment,
    ) -> impl FutureMaybeSend<Option<Arc<PublicKey>>> {
        async { todo!() }
    }
}

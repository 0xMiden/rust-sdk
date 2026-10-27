use miden_client_core::errors::RpcError;
use miden_client_core::rpc::RpcEndpoint;
use miden_client_core::rpc::encryption::{
    AttestedTransactionEncryptionKey,
    NextTransactionEncryptionKey,
    ValidatorAttestation,
};
use miden_objects::DecodeMessageExt;
use tracing::warn;

use crate::{GrpcClient, RpcMethod, generated as proto};

/// Fetches the key that encrypts transaction inputs for the node, with its validator attestations.
pub struct GetTransactionEncryptionKey;

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl RpcMethod for GetTransactionEncryptionKey {
    type Output = AttestedTransactionEncryptionKey;

    async fn execute(&self, client: &GrpcClient) -> Result<Self::Output, RpcError> {
        let response = client
            .call_with_retry(RpcEndpoint::GetTransactionEncryptionKey, |mut rpc_api| {
                Box::pin(async move {
                    rpc_api
                        .get_transaction_encryption_key(
                            proto::node::v1::GetTransactionEncryptionKeyRequest {},
                        )
                        .await
                })
            })
            .await?
            .into_inner()
            .key
            .ok_or(RpcError::ExpectedDataMissing("TransactionEncryptionKey".to_owned()))?;

        // An undecodable attestation is skipped rather than failing the whole response, so that one
        // junk entry served by the relaying operator cannot hide a valid attestation behind it.
        // Verification requires one that both decodes and verifies, so dropping the rest is safe.
        let attestations = response
            .attestations
            .into_iter()
            .filter_map(|attestation| {
                let validator_key =
                    attestation.validator_public_key.and_then(|key| key.decode_and_verify().ok());
                let signature =
                    attestation.signature.and_then(|signature| signature.decode_and_verify().ok());
                let decoded = validator_key.zip(signature).map(|(validator_key, signature)| {
                    ValidatorAttestation { validator_key, signature }
                });
                if decoded.is_none() {
                    warn!(
                        "skipping a transaction encryption key attestation that failed to decode"
                    );
                }
                decoded
            })
            .collect::<Vec<_>>();

        // A negative scheme is a malformed response, not a scheme this client happens to not
        // support, so it is rejected here rather than aliased onto a valid identifier.
        let wire_scheme = |scheme: i32| {
            u32::try_from(scheme)
                .map_err(|_| RpcError::InvalidResponse(format!("negative IES scheme '{scheme}'")))
        };

        let next_key = response
            .next_key
            .map(|next| {
                Ok::<_, RpcError>(NextTransactionEncryptionKey {
                    scheme: wire_scheme(next.scheme)?,
                    key_id: next.key_id,
                    public_key: next.public_key,
                    rotation_block_num: next.rotation_block_num.into(),
                })
            })
            .transpose()?;

        Ok(AttestedTransactionEncryptionKey {
            scheme: wire_scheme(response.scheme)?,
            key_id: response.key_id,
            public_key: response.public_key,
            attestations,
            next_key,
        })
    }

    fn validate(&self, _output: &Self::Output) -> Result<(), RpcError> {
        // The request carries no payload to check the response against. Trust in the served key
        // comes from the validator attestation, which the caller verifies via
        // `AttestedTransactionEncryptionKey::verify`.
        Ok(())
    }
}

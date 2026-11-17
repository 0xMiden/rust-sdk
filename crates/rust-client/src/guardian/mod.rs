//! Transport to the Guardian service that guards the accounts of the client.
//!
//! Every request is signed by a cosigner of the account. The signed message commits to the account,
//! a timestamp, and the encoded request.

mod generated;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use generated::guardian_client::GuardianClient as GuardianApi;
use generated::{ExecuteDeltaProposalRequest, GetStateRequest};
use miden_protocol::account::auth::{PublicKey, PublicKeyCommitment, Signature};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::crypto::hash::rpo::Rpo256;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Felt, Word};
use miden_tx::auth::{SigningInputs, TransactionAuthenticator};
use tonic::metadata::MetadataValue;
#[cfg(not(target_arch = "wasm32"))]
use tonic::transport::{Channel, ClientTlsConfig};

use crate::transaction::{GuardianError, GuardianExecution, GuardianExecutionState};

#[cfg(not(target_arch = "wasm32"))]
type Service = Channel;
#[cfg(target_arch = "wasm32")]
type Service = tonic_web_wasm_client::Client;

/// Prefix of the signed payload of an execution request, so that its signature never authenticates
/// another request with the same body.
const EXECUTE_TAG: &[u8] = b"guardian.ExecuteDeltaProposal\0";

// GUARDIAN CLIENT
// ================================================================================================

/// gRPC client for the Guardian service.
pub(crate) struct GuardianClient {
    endpoint: String,
    /// The timestamp of the last signed request, in milliseconds. Guardian rejects a timestamp that
    /// is not greater than the last one it accepted from the same signer.
    last_timestamp_ms: AtomicU64,
}

impl GuardianClient {
    pub(crate) fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            last_timestamp_ms: AtomicU64::new(0),
        }
    }

    /// Returns the account state that Guardian holds for `account_id`.
    pub(crate) async fn get_state<A: TransactionAuthenticator>(
        &self,
        account_id: AccountId,
        authenticator: &A,
        cosigner: PublicKeyCommitment,
        now_ms: u64,
    ) -> Result<Account, GuardianError> {
        let message = GetStateRequest { account_id: account_id.to_hex() };
        let request = self.sign(message, &[], account_id, authenticator, cosigner, now_ms).await?;

        let response =
            self.connect().await?.get_state(request).await.map_err(refused)?.into_inner();
        if !response.success {
            return Err(GuardianError::Refused {
                code: String::from("GetState"),
                message: response.message,
            });
        }

        let state = response.state.ok_or_else(|| invalid("missing account state"))?;
        decode_account(&state.state_json)
    }

    /// Asks Guardian to execute the signed summary with commitment `summary_commitment`.
    pub(crate) async fn execute<A: TransactionAuthenticator>(
        &self,
        account_id: AccountId,
        summary_commitment: Word,
        authenticator: &A,
        cosigner: PublicKeyCommitment,
        now_ms: u64,
    ) -> Result<GuardianExecution, GuardianError> {
        let message = ExecuteDeltaProposalRequest {
            account_id: account_id.to_hex(),
            proposal_id: to_hex(&summary_commitment.as_bytes()),
        };
        let request = self
            .sign(message, EXECUTE_TAG, account_id, authenticator, cosigner, now_ms)
            .await?;

        let execution = self
            .connect()
            .await?
            .execute_delta_proposal(request)
            .await
            .map_err(refused)?
            .into_inner()
            .execution
            .ok_or_else(|| invalid("missing execution"))?;

        Ok(GuardianExecution {
            proposal_id: summary_commitment,
            state: parse_execution_state(&execution.state)?,
        })
    }

    /// Wraps `message` in a request that carries the signature of `cosigner` in its metadata.
    async fn sign<T: prost::Message, A: TransactionAuthenticator>(
        &self,
        message: T,
        tag: &[u8],
        account_id: AccountId,
        authenticator: &A,
        cosigner: PublicKeyCommitment,
        now_ms: u64,
    ) -> Result<tonic::Request<T>, GuardianError> {
        let mut payload = tag.to_vec();
        payload.extend(message.encode_to_vec());

        let timestamp_ms = self.next_timestamp(now_ms);
        let signed_word = request_word(account_id, timestamp_ms, payload_word(&payload));

        let signature = authenticator
            .get_signature(cosigner, &SigningInputs::Blind(signed_word))
            .await
            .map_err(GuardianError::Signing)?;
        let public_key = authenticator
            .get_public_key(cosigner)
            .await
            .ok_or(GuardianError::MissingAuthenticator)?;

        let mut request = tonic::Request::new(message);
        let metadata = request.metadata_mut();
        metadata.insert("x-pubkey", ascii(public_key_hex(&public_key)?)?);
        metadata.insert("x-signature", ascii(signature_hex(&signature))?);
        metadata.insert("x-timestamp", ascii(timestamp_ms.to_string())?);

        Ok(request)
    }

    /// Returns a timestamp greater than the last one this client signed.
    fn next_timestamp(&self, now_ms: u64) -> u64 {
        let previous = self
            .last_timestamp_ms
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| Some(now_ms.max(last + 1)))
            .expect("the update function always returns a value");
        now_ms.max(previous + 1)
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn connect(&self) -> Result<GuardianApi<Service>, GuardianError> {
        let channel = tonic::transport::Endpoint::try_from(self.endpoint.clone())
            .map_err(connection)?
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(connection)?
            .connect()
            .await
            .map_err(connection)?;
        Ok(GuardianApi::new(channel))
    }

    #[cfg(target_arch = "wasm32")]
    #[allow(clippy::unused_async)]
    async fn connect(&self) -> Result<GuardianApi<Service>, GuardianError> {
        Ok(GuardianApi::new(tonic_web_wasm_client::Client::new(self.endpoint.clone())))
    }
}

// REQUEST SIGNING
// ================================================================================================

/// Hashes the request bytes as 8-byte little-endian chunks, the last one zero-padded. Empty bytes
/// give the empty word.
fn payload_word(bytes: &[u8]) -> Word {
    if bytes.is_empty() {
        return Word::empty();
    }

    let elements: Vec<Felt> = bytes
        .chunks(8)
        .map(|chunk| {
            let mut chunk_bytes = [0u8; 8];
            chunk_bytes[..chunk.len()].copy_from_slice(chunk);
            reduced_felt(u64::from_le_bytes(chunk_bytes))
        })
        .collect();
    Rpo256::hash_elements(&elements)
}

/// Returns the word the cosigner signs: `hash(account_prefix, account_suffix, timestamp, PAYLOAD)`.
fn request_word(account_id: AccountId, timestamp_ms: u64, payload: Word) -> Word {
    let mut elements = Vec::with_capacity(7);
    elements.push(account_id.prefix().as_felt());
    elements.push(account_id.suffix());
    elements.push(reduced_felt(timestamp_ms));
    elements.extend_from_slice(payload.as_elements());
    Rpo256::hash_elements(&elements)
}

fn reduced_felt(value: u64) -> Felt {
    Felt::new(value % Felt::ORDER).expect("a value reduced modulo the field order is canonical")
}

/// Encodes the inner key of `public_key`, the format that Guardian reads from `x-pubkey`.
fn public_key_hex(public_key: &PublicKey) -> Result<String, GuardianError> {
    match public_key {
        PublicKey::Falcon512Poseidon2(key) => Ok(to_hex(&key.to_bytes())),
        PublicKey::EcdsaK256Keccak(key) => Ok(to_hex(&key.to_bytes())),
        _ => Err(GuardianError::UnsupportedKeyScheme),
    }
}

/// Encodes the inner signature of `signature`, the format that Guardian reads from `x-signature`.
fn signature_hex(signature: &Signature) -> String {
    match signature {
        Signature::Falcon512Poseidon2(signature) => to_hex(&signature.to_bytes()),
        Signature::EcdsaK256Keccak(signature) => to_hex(&signature.to_bytes()),
    }
}

// HELPERS
// ================================================================================================

/// Decodes the account of Guardian's `state_json`: `{"data": base64(account bytes), ..}`.
fn decode_account(state_json: &str) -> Result<Account, GuardianError> {
    let state: serde_json::Value =
        serde_json::from_str(state_json).map_err(|err| invalid(&err.to_string()))?;
    let data = state
        .get("data")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| invalid("missing account data"))?;
    let bytes = STANDARD.decode(data).map_err(|err| invalid(&err.to_string()))?;

    Account::read_from_bytes(&bytes).map_err(|err| invalid(&err.to_string()))
}

fn parse_execution_state(state: &str) -> Result<GuardianExecutionState, GuardianError> {
    match state {
        "pending" => Ok(GuardianExecutionState::Pending),
        "proving" => Ok(GuardianExecutionState::Proving),
        "submitted" => Ok(GuardianExecutionState::Submitted),
        "committed" => Ok(GuardianExecutionState::Committed),
        "failed" => Ok(GuardianExecutionState::Failed),
        other => Err(invalid(other)),
    }
}

/// Encodes `bytes` as `0x` followed by lowercase hex.
fn to_hex(bytes: &[u8]) -> String {
    let mut hex = String::from("0x");
    for byte in bytes {
        write!(hex, "{byte:02x}").expect("writing to a string does not fail");
    }
    hex
}

fn ascii(value: String) -> Result<MetadataValue<tonic::metadata::Ascii>, GuardianError> {
    MetadataValue::try_from(value).map_err(|err| invalid(&err.to_string()))
}

fn refused(status: tonic::Status) -> GuardianError {
    GuardianError::Refused {
        code: format!("{:?}", status.code()),
        message: String::from(status.message()),
    }
}

fn connection(err: impl core::error::Error + Send + Sync + 'static) -> GuardianError {
    GuardianError::Connection(Box::new(err))
}

fn invalid(reason: &str) -> GuardianError {
    GuardianError::InvalidResponse(String::from(reason))
}

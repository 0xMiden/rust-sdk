//! gRPC client for Guardian.
//!
//! A cosigner of the account signs every request. The signed message commits to the account, a
//! timestamp, and the encoded request.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use miden_protocol::account::auth::{PublicKey, PublicKeyCommitment, Signature};
use miden_protocol::account::{Account, AccountId};
use miden_protocol::crypto::hash::rpo::Rpo256;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Felt, Word};
use miden_tx::auth::{SigningInputs, TransactionAuthenticator};
use serde::Deserialize;
use tonic::metadata::AsciiMetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Code, Status};

use crate::errors::GuardedClientError;
use crate::generated::guardian_client::GuardianClient;
use crate::generated::{
    ExecuteDeltaProposalRequest,
    ExecutionEnvelope,
    GetDeltaProposalExecutionRequest,
    GetStateRequest,
};
use crate::{ExecutionFailure, GuardianExecution, GuardianExecutionState};

/// Prefix of the signed payload of an execution request, so that its signature never authenticates
/// another request with the same body.
const EXECUTE_TAG: &[u8] = b"guardian.ExecuteDeltaProposal\0";

/// Refusal code of a request whose timestamp is not above the last one Guardian accepted from the
/// same signer.
const REPLAY_CODE: &str = "authentication_replay";

const MAX_RETRIES: usize = 4;
/// Delay before a retry when Guardian sends no `retry-after` value.
const RETRY_INTERVAL: Duration = Duration::from_millis(100);

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

// GRPC GUARDIAN CLIENT
// ================================================================================================

/// gRPC client for the Guardian service, bound to one account and one cosigner key.
///
/// Each request opens a new connection, because Guardian requests are infrequent.
pub(crate) struct GrpcGuardianClient<AUTH> {
    endpoint: String,
    account_id: AccountId,
    cosigner: PublicKeyCommitment,
    authenticator: Arc<AUTH>,
    /// The timestamp of the last signed request, in milliseconds. Guardian rejects a timestamp that
    /// is not greater than the last one it accepted from the same signer.
    last_timestamp_ms: AtomicU64,
}

impl<AUTH: TransactionAuthenticator> GrpcGuardianClient<AUTH> {
    pub(crate) fn new(
        endpoint: String,
        account_id: AccountId,
        cosigner: PublicKeyCommitment,
        authenticator: Arc<AUTH>,
    ) -> Self {
        Self {
            endpoint,
            account_id,
            cosigner,
            authenticator,
            last_timestamp_ms: AtomicU64::new(0),
        }
    }

    /// Opens a connection to Guardian.
    async fn connect(&self) -> Result<GuardianClient<Channel>, GuardedClientError> {
        let channel = Endpoint::from_shared(self.endpoint.clone())
            .map_err(GuardedClientError::Connection)?
            .timeout(REQUEST_TIMEOUT)
            .tls_config(ClientTlsConfig::new().with_native_roots())
            .map_err(GuardedClientError::Connection)?
            .connect()
            .await
            .map_err(GuardedClientError::Connection)?;
        Ok(GuardianClient::new(channel))
    }

    /// Returns the account state that Guardian holds.
    pub(crate) async fn get_state(&self) -> Result<Account, GuardedClientError> {
        let message = GetStateRequest { account_id: self.account_id.to_hex() };
        let response = self
            .call(message, &[], |mut api, request| async move { api.get_state(request).await })
            .await?;

        let state = response.state.ok_or_else(|| invalid("missing account state"))?;
        decode_account(&state.state_json)
    }

    /// Asks Guardian to execute the proposal with ID `proposal_id`.
    pub(crate) async fn execute(
        &self,
        proposal_id: Word,
    ) -> Result<GuardianExecution, GuardedClientError> {
        let message = ExecuteDeltaProposalRequest {
            account_id: self.account_id.to_hex(),
            proposal_id: to_hex(&proposal_id.as_bytes()),
        };
        let response = self
            .call(message, EXECUTE_TAG, |mut api, request| async move {
                api.execute_delta_proposal(request).await
            })
            .await?;

        parse_execution(proposal_id, response.execution)
    }

    /// Returns the latest execution of the proposal with ID `proposal_id`.
    pub(crate) async fn execution_status(
        &self,
        proposal_id: Word,
    ) -> Result<GuardianExecution, GuardedClientError> {
        let message = GetDeltaProposalExecutionRequest {
            account_id: self.account_id.to_hex(),
            proposal_id: to_hex(&proposal_id.as_bytes()),
        };
        let response = self
            .call(message, &[], |mut api, request| async move {
                api.get_delta_proposal_execution(request).await
            })
            .await?;

        parse_execution(proposal_id, response.execution)
    }

    /// Signs `message` and sends it with `send`.
    ///
    /// A retryable refusal is sent again, up to [`MAX_RETRIES`] times. Every attempt is signed
    /// again, because a replay refusal needs a new timestamp: another client that uses the same key
    /// can move the timestamp floor.
    async fn call<T, R, Fut>(
        &self,
        message: T,
        tag: &[u8],
        send: impl Fn(GuardianClient<Channel>, tonic::Request<T>) -> Fut,
    ) -> Result<R, GuardedClientError>
    where
        T: prost::Message + Clone,
        Fut: Future<Output = Result<tonic::Response<R>, Status>>,
    {
        let mut retries = 0;
        loop {
            let request = self.sign(message.clone(), tag).await?;
            let status = match send(self.connect().await?, request).await {
                Ok(response) => return Ok(response.into_inner()),
                Err(status) => status,
            };

            let error = refused(&status);
            let Some(delay) = retry_delay(&error).filter(|_| retries < MAX_RETRIES) else {
                return Err(error);
            };
            retries += 1;
            tokio::time::sleep(delay).await;
        }
    }

    /// Wraps `message` in a request that carries the signature of the cosigner in its metadata.
    async fn sign<T: prost::Message>(
        &self,
        message: T,
        tag: &[u8],
    ) -> Result<tonic::Request<T>, GuardedClientError> {
        let mut payload = tag.to_vec();
        payload.extend(message.encode_to_vec());

        let timestamp_ms = self.next_timestamp();
        let signed_word = request_word(self.account_id, timestamp_ms, payload_word(&payload));

        let signature = self
            .authenticator
            .get_signature(self.cosigner, &SigningInputs::Blind(signed_word))
            .await
            .map_err(GuardedClientError::Signing)?;
        let public_key = self
            .authenticator
            .get_public_key(self.cosigner)
            .await
            .ok_or(GuardedClientError::CosignerKeyNotHeld)?;

        let mut request = tonic::Request::new(message);
        let metadata = request.metadata_mut();
        metadata.insert("x-pubkey", ascii(public_key_hex(&public_key)?)?);
        metadata.insert("x-signature", ascii(signature_hex(&signature))?);
        metadata.insert("x-timestamp", ascii(timestamp_ms.to_string())?);

        Ok(request)
    }

    /// Returns a timestamp greater than the last one this client signed.
    fn next_timestamp(&self) -> u64 {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX));
        let previous = self
            .last_timestamp_ms
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |last| Some(now_ms.max(last + 1)))
            .expect("the update function always returns a value");
        now_ms.max(previous + 1)
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
fn public_key_hex(public_key: &PublicKey) -> Result<String, GuardedClientError> {
    match public_key {
        PublicKey::Falcon512Poseidon2(key) => Ok(to_hex(&key.to_bytes())),
        PublicKey::EcdsaK256Keccak(key) => Ok(to_hex(&key.to_bytes())),
        _ => Err(GuardedClientError::UnsupportedKeyScheme),
    }
}

/// Encodes the inner signature of `signature`, the format that Guardian reads from `x-signature`.
fn signature_hex(signature: &Signature) -> String {
    match signature {
        Signature::Falcon512Poseidon2(signature) => to_hex(&signature.to_bytes()),
        Signature::EcdsaK256Keccak(signature) => to_hex(&signature.to_bytes()),
    }
}

// RESPONSES
// ================================================================================================

/// The JSON that Guardian puts in the details of a refusal.
#[derive(Deserialize)]
struct RefusalDetails {
    code: String,
    message: String,
    #[serde(default)]
    meta: RefusalMeta,
}

#[derive(Default, Deserialize)]
struct RefusalMeta {
    #[serde(default)]
    retryable: bool,
}

/// The JSON of `AccountState.state_json`. `data` is the base64 encoding of the account bytes.
#[derive(Deserialize)]
struct StateJson {
    data: String,
}

fn refused(status: &Status) -> GuardedClientError {
    let retry_after_secs = extract_retry_after_secs(status);
    match serde_json::from_slice::<RefusalDetails>(status.details()) {
        Ok(details) => GuardedClientError::Refused {
            code: details.code,
            message: details.message,
            retryable: details.meta.retryable,
            retry_after_secs,
        },
        // Transport failures and refusals without details carry only the gRPC status.
        Err(_) => GuardedClientError::Refused {
            code: format!("{:?}", status.code()),
            message: status.message().to_owned(),
            retryable: matches!(status.code(), Code::Unavailable | Code::ResourceExhausted),
            retry_after_secs,
        },
    }
}

/// Returns the delay before the request is sent again, or `None` if a retry cannot succeed.
///
/// A replay refusal needs only a new timestamp. Other retryable refusals wait for the delay that
/// Guardian asks for.
fn retry_delay(error: &GuardedClientError) -> Option<Duration> {
    match error {
        GuardedClientError::Refused { code, .. } if code == REPLAY_CODE => Some(RETRY_INTERVAL),
        GuardedClientError::Refused { retryable: true, retry_after_secs, .. } => {
            Some(retry_after_secs.map_or(RETRY_INTERVAL, Duration::from_secs))
        },
        _ => None,
    }
}

/// Returns the delay that the `retry-after` metadata value of `status` requests, in whole seconds.
fn extract_retry_after_secs(status: &Status) -> Option<u64> {
    status
        .metadata()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

fn parse_execution(
    proposal_id: Word,
    envelope: Option<ExecutionEnvelope>,
) -> Result<GuardianExecution, GuardedClientError> {
    let envelope = envelope.ok_or_else(|| invalid("missing execution"))?;
    let state = match envelope.state.as_str() {
        "pending" => GuardianExecutionState::Pending,
        "proving" => GuardianExecutionState::Proving,
        "submitted" => GuardianExecutionState::Submitted,
        "committed" => GuardianExecutionState::Committed,
        "failed" => {
            let error = envelope.error.ok_or_else(|| invalid("failed execution without error"))?;
            GuardianExecutionState::Failed(ExecutionFailure {
                code: error.code,
                message: error.message,
                meta_json: error.meta_json,
            })
        },
        other => return Err(invalid(&format!("unknown execution state {other}"))),
    };

    Ok(GuardianExecution { proposal_id, state })
}

fn decode_account(state_json: &str) -> Result<Account, GuardedClientError> {
    let state: StateJson =
        serde_json::from_str(state_json).map_err(|err| invalid(&err.to_string()))?;
    let bytes = STANDARD.decode(state.data).map_err(|err| invalid(&err.to_string()))?;
    Account::read_from_bytes(&bytes).map_err(|err| invalid(&err.to_string()))
}

// HELPERS
// ================================================================================================

/// Encodes `bytes` as `0x` followed by lowercase hex.
fn to_hex(bytes: &[u8]) -> String {
    format!("0x{}", hex::encode(bytes))
}

fn ascii(value: String) -> Result<AsciiMetadataValue, GuardedClientError> {
    AsciiMetadataValue::try_from(value).map_err(|err| invalid(&err.to_string()))
}

fn invalid(reason: &str) -> GuardedClientError {
    GuardedClientError::InvalidResponse(reason.to_owned())
}

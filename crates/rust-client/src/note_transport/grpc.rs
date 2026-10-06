//! gRPC-based note transport client.
//!
//! On native targets, the connection is established lazily on the first request using a TLS-enabled
//! `tonic` channel. On WASM, a `tonic_web_wasm_client` is created on demand.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use miden_objects::{
    ConversionError,
    ConversionResultExt,
    DecodeMessage,
    DecodeMessageExt,
    Verify,
};
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{
    NoteDetails,
    NoteDetailsCommitment,
    NoteHeader,
    NoteInclusionProof,
    NoteTag,
};
use miden_protocol::utils::serde::Serializable;
use miden_tx::utils::sync::RwLock;
use thiserror::Error;
use tonic::{Code, Request, Status};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;
#[cfg(target_arch = "wasm32")]
use {core::time::Duration, tonic_web_wasm_client::options::FetchOptions};
#[cfg(not(target_arch = "wasm32"))]
use {
    std::time::Duration,
    tonic::transport::{Channel, ClientTlsConfig},
};

use super::generated::note_transport::note_transport_service_client::NoteTransportServiceClient;
use super::generated::note_transport::{
    FetchNotesCursor,
    FetchNotesRequest,
    FetchedNote,
    SendNoteWithProofRequest,
    TransportNote as ProtoTransportNote,
};
use super::{NoteInfo, NoteTransportCursor, NoteTransportError, NoteTransportPage, TransportNote};
use crate::grpc_support::{async_sleep, extract_retry_after};

// FETCHED NOTE DECODING
// ================================================================================================

/// The decoded fields of a [`FetchedNote`], before the details are checked against the header.
pub struct DecodedFetchedNote {
    header: NoteHeader,
    details: NoteDetails,
    block_hint: Option<BlockNumber>,
}

impl TryFrom<FetchedNote> for DecodedFetchedNote {
    type Error = ConversionError;

    fn try_from(note: FetchedNote) -> Result<Self, Self::Error> {
        let header = note
            .header
            .ok_or_else(|| ConversionError::missing_field::<FetchedNote>("header"))?
            .decode_and_verify()
            .context("header")?;
        let details = note
            .details
            .ok_or_else(|| ConversionError::missing_field::<FetchedNote>("details"))?
            .decode_and_verify()
            .context("details")?;
        let block_hint =
            note.committed_in_block.map(|block_num| BlockNumber::from(block_num.block_num));

        Ok(Self { header, details, block_hint })
    }
}

impl DecodeMessage for FetchedNote {
    type Decoded = DecodedFetchedNote;
}

/// The details of a fetched note do not match the commitment its header carries.
#[derive(Debug, Error)]
#[error(
    "fetched note details (commitment {}) do not match the header's details commitment {}",
    details.to_hex(),
    header.to_hex()
)]
pub struct FetchedNoteMismatch {
    header: NoteDetailsCommitment,
    details: NoteDetailsCommitment,
}

impl Verify for DecodedFetchedNote {
    type Verified = NoteInfo;
    type Error = FetchedNoteMismatch;

    /// Checks that the header commits to the delivered details.
    fn verify(self) -> Result<NoteInfo, FetchedNoteMismatch> {
        if self.details.commitment() != self.header.details_commitment() {
            return Err(FetchedNoteMismatch {
                header: self.header.details_commitment(),
                details: self.details.commitment(),
            });
        }

        Ok(NoteInfo {
            header: self.header,
            details_bytes: self.details.to_bytes(),
            block_hint: self.block_hint,
        })
    }
}

/// Builds the wire representation of a transport note.
fn proto_transport_note(note: TransportNote) -> ProtoTransportNote {
    let (header, details) = note.into_parts();
    ProtoTransportNote {
        header: Some(header.into()),
        details: Some(details.into()),
    }
}

// GRPC CLIENT
// ================================================================================================

#[cfg(not(target_arch = "wasm32"))]
type Service = Channel;
#[cfg(target_arch = "wasm32")]
type Service = tonic_web_wasm_client::Client;

/// Establishes a connection to the note transport service with the configured channel timeout.
#[cfg(not(target_arch = "wasm32"))]
async fn connect_channel(
    endpoint: &str,
    timeout_ms: u64,
) -> Result<ConnectedClient, NoteTransportError> {
    let endpoint = tonic::transport::Endpoint::try_from(String::from(endpoint))
        .map_err(|e| NoteTransportError::Connection(Box::new(e)))?
        .timeout(Duration::from_millis(timeout_ms));
    let tls = ClientTlsConfig::new().with_native_roots();
    let channel = endpoint
        .tls_config(tls)
        .map_err(|e| NoteTransportError::Connection(Box::new(e)))?
        .connect()
        .await
        .map_err(|e| NoteTransportError::Connection(Box::new(e)))?;
    Ok(ConnectedClient {
        client: NoteTransportServiceClient::new(channel.clone()),
        health_client: HealthClient::new(channel),
    })
}

/// Establishes note transport clients with timed requests.
#[cfg(target_arch = "wasm32")]
#[allow(clippy::unused_async)]
async fn connect_channel(
    endpoint: &str,
    timeout_ms: u64,
) -> Result<ConnectedClient, NoteTransportError> {
    let fetch_options = FetchOptions::new().timeout(Duration::from_millis(timeout_ms));
    let wasm_client =
        tonic_web_wasm_client::Client::new_with_options(String::from(endpoint), fetch_options);
    Ok(ConnectedClient {
        client: NoteTransportServiceClient::new(wasm_client.clone()),
        health_client: HealthClient::new(wasm_client),
    })
}

/// Inner state holding the connected gRPC clients.
#[derive(Clone)]
struct ConnectedClient {
    client: NoteTransportServiceClient<Service>,
    health_client: HealthClient<Service>,
}

/// gRPC client for the note transport network.
///
/// The connection is established lazily on first use. A send that fails with a transient error is
/// retried a bounded number of times, see [`GrpcNoteTransportClient::send_note_with_proof`].
pub struct GrpcNoteTransportClient {
    inner: RwLock<Option<ConnectedClient>>,
    endpoint: String,
    timeout_ms: u64,
    /// Maximum number of retries of a send after a transient failure.
    max_retries: u32,
    /// Delay before the first retry of a send, in milliseconds.
    retry_interval_ms: u64,
}

impl GrpcNoteTransportClient {
    /// Creates a new [`GrpcNoteTransportClient`] without establishing a connection. The connection
    /// will be established lazily on the first request.
    pub fn new(endpoint: String, timeout_ms: u64) -> Self {
        Self {
            inner: RwLock::new(None),
            endpoint,
            timeout_ms,
            max_retries: DEFAULT_SEND_MAX_RETRIES,
            retry_interval_ms: DEFAULT_SEND_RETRY_INTERVAL_MS,
        }
    }

    /// Sets the maximum number of retries of a send after a transient failure. Defaults to `3`. A
    /// value of `0` disables the retries.
    #[must_use]
    pub fn with_max_retries(mut self, max_retries: u32) -> Self {
        self.max_retries = max_retries;
        self
    }

    /// Sets the delay before the first retry of a send, in milliseconds. Each subsequent retry
    /// waits twice as long as the retry before it. A `retry-after` value from the service replaces
    /// this delay. Defaults to `250` ms.
    #[must_use]
    pub fn with_retry_interval_ms(mut self, retry_interval_ms: u64) -> Self {
        self.retry_interval_ms = retry_interval_ms;
        self
    }

    /// Ensures the client is connected and returns the connected state.
    async fn ensure_connected(&self) -> Result<ConnectedClient, NoteTransportError> {
        if let Some(connected) = self.inner.read().as_ref() {
            return Ok(connected.clone());
        }

        let connected = connect_channel(&self.endpoint, self.timeout_ms).await?;
        *self.inner.write() = Some(connected.clone());
        Ok(connected)
    }

    /// Get a clone of the main client, connecting if needed.
    async fn api(&self) -> Result<NoteTransportServiceClient<Service>, NoteTransportError> {
        Ok(self.ensure_connected().await?.client)
    }

    /// Get a clone of the health client, connecting if needed.
    async fn health_api(&self) -> Result<HealthClient<Service>, NoteTransportError> {
        Ok(self.ensure_connected().await?.health_client)
    }

    /// Pushes a note to the note transport network together with its inclusion proof.
    ///
    /// The service verifies the proof against its node before it stores the note. It relays the
    /// commitment block to recipients as the exact inclusion block.
    ///
    /// The service stores a note only once and returns success for a note it already stores. A
    /// repeated send is therefore safe, and this method retries a send that fails with a transient
    /// error. The retries stop after the limit that [`Self::with_max_retries`] sets. The method
    /// then returns the last error.
    pub async fn send_note_with_proof(
        &self,
        note: TransportNote,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), NoteTransportError> {
        let note_id = note.header().id();
        let request = SendNoteWithProofRequest {
            inclusion_proof: Some((&note_id, &inclusion_proof).into()),
            note: Some(proto_transport_note(note)),
        };

        send_with_retry(self.max_retries, self.retry_interval_ms, || {
            let request = request.clone();
            async move {
                let mut api = self.api().await.map_err(SendFailure::Connect)?;
                api.send_note_with_proof(Request::new(request))
                    .await
                    .map_err(SendFailure::Status)?;
                Ok(())
            }
        })
        .await
    }

    /// Downloads notes for given tags from the note transport network.
    ///
    /// Returns notes labeled after the provided cursor (pagination), and an updated cursor.
    pub async fn fetch_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError> {
        let page = self.fetch_notes_page(tags, cursor).await?;
        Ok((page.notes, page.cursor))
    }

    /// Fetches one page with the service's continuation flag.
    pub async fn fetch_notes_page(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<NoteTransportPage, NoteTransportError> {
        let tags_int = tags.iter().map(NoteTag::as_u32).collect();
        let request = FetchNotesRequest {
            tags: tags_int,
            cursor: cursor.parts().map(|(nonce, sequence)| FetchNotesCursor { nonce, sequence }),
        };

        let mut api = self.api().await?;
        let response = match api.fetch_notes(Request::new(request.clone())).await {
            Ok(response) => response,
            Err(status)
                if status.code() == Code::FailedPrecondition && request.cursor.is_some() =>
            {
                let retry = FetchNotesRequest { cursor: None, ..request };
                api.fetch_notes(Request::new(retry)).await.map_err(|error| {
                    NoteTransportError::Network(format!("Fetch notes failed: {error:?}"))
                })?
            },
            Err(error) => {
                return Err(NoteTransportError::Network(format!("Fetch notes failed: {error:?}")));
            },
        };

        let response = response.into_inner();

        // The service rejects notes that do not decode or whose details do not match their header.
        // A fetched note that fails these checks shows that the service misbehaves, so the fetch
        // fails.
        let notes = response
            .notes
            .into_iter()
            .map(|note| note.decode_and_verify().map_err(NoteTransportError::InvalidFetchedNote))
            .collect::<Result<Vec<_>, _>>()?;

        let cursor = response
            .cursor
            .ok_or_else(|| NoteTransportError::Network("fetch response has no cursor".into()))?;
        Ok(NoteTransportPage {
            notes,
            cursor: NoteTransportCursor::from_parts(cursor.nonce, cursor.sequence),
            has_more: response.has_more,
        })
    }

    /// gRPC-standardized server health-check.
    ///
    /// Checks if the note transport node and respective gRPC services are serving requests. The
    /// gRPC server operates the `note_transport.Api` service.
    pub async fn health_check(&mut self) -> Result<(), NoteTransportError> {
        let request = tonic::Request::new(HealthCheckRequest {
            service: String::new(), // empty string -> whole server
        });

        let response = self
            .health_api()
            .await?
            .check(request)
            .await
            .map_err(|e| NoteTransportError::Network(format!("Health check failed: {e}")))?
            .into_inner();

        let serving = matches!(
            response.status(),
            tonic_health::pb::health_check_response::ServingStatus::Serving
        );

        serving
            .then_some(())
            .ok_or_else(|| NoteTransportError::Network("Service is not serving".into()))
    }
}
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl super::NoteTransportClient for GrpcNoteTransportClient {
    async fn send_note_with_proof(
        &self,
        note: TransportNote,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), NoteTransportError> {
        self.send_note_with_proof(note, inclusion_proof).await
    }

    async fn fetch_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError> {
        self.fetch_notes(tags, cursor).await
    }

    async fn fetch_notes_page(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<NoteTransportPage, NoteTransportError> {
        self.fetch_notes_page(tags, cursor).await
    }
}

// SEND RETRY
// ================================================================================================

/// Default maximum number of retries of a send after a transient failure.
const DEFAULT_SEND_MAX_RETRIES: u32 = 3;

/// Default delay before the first retry of a send, in milliseconds.
const DEFAULT_SEND_RETRY_INTERVAL_MS: u64 = 250;

/// The reason that one send attempt failed.
#[derive(Debug)]
enum SendFailure {
    /// The client could not connect to the service. The request did not reach the service.
    Connect(NoteTransportError),
    /// The call returned an error status, from the service or from the local gRPC stack.
    Status(Status),
}

impl SendFailure {
    fn into_error(self) -> NoteTransportError {
        match self {
            Self::Connect(err) => err,
            Self::Status(status) => {
                NoteTransportError::Network(format!("Send note with proof failed: {status:?}"))
            },
        }
    }
}

/// Runs `attempt` until it succeeds, until it fails with an error that a retry cannot fix, or until
/// `max_retries` retries have failed. Returns the error of the last attempt.
async fn send_with_retry<F, Fut>(
    max_retries: u32,
    retry_interval_ms: u64,
    mut attempt: F,
) -> Result<(), NoteTransportError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<(), SendFailure>>,
{
    let mut retry = 0;
    loop {
        let failure = match attempt().await {
            Ok(()) => return Ok(()),
            Err(failure) => failure,
        };

        let delay = if retry < max_retries {
            retry_delay(&failure, retry, retry_interval_ms)
        } else {
            None
        };
        let Some(delay) = delay else {
            return Err(failure.into_error());
        };

        tracing::warn!(
            ?failure,
            retry = retry + 1,
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "note transport send failed, retrying after delay",
        );
        async_sleep(delay).await;
        retry += 1;
    }
}

/// Returns the delay before retry number `retry` (counted from zero) of a send that failed with
/// `failure`. Returns `None` when a retry cannot get a different result.
///
/// The service stores a note only once, so a retry is safe also when an earlier attempt reached the
/// service. These failures are retried:
///
/// - A failed connection. The request did not reach the service.
/// - `Unavailable`. The local gRPC stack returns it when the connection breaks. The service returns
///   it when it cannot reach its node.
/// - `DeadlineExceeded`. The service returns it when its node does not answer in time.
/// - `ResourceExhausted` with a `retry-after` value, which a rate limiter returns. Without that
///   value, the service rejects a note that is too large or reports that its storage is full. A
///   retry gets the same result.
///
/// The delay starts at `retry_interval_ms` and doubles with each retry. A non-zero `retry-after`
/// value replaces it.
fn retry_delay(failure: &SendFailure, retry: u32, retry_interval_ms: u64) -> Option<Duration> {
    let backoff =
        Duration::from_millis(retry_interval_ms.saturating_mul(2u64.saturating_pow(retry)));
    let status = match failure {
        SendFailure::Connect(_) => return Some(backoff),
        SendFailure::Status(status) => status,
    };

    let retry_after = extract_retry_after(status);
    let retryable = match status.code() {
        Code::Unavailable | Code::DeadlineExceeded => true,
        Code::ResourceExhausted => retry_after.is_some(),
        _ => false,
    };

    retryable.then(|| retry_after.filter(|delay| !delay.is_zero()).unwrap_or(backoff))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use miden_protocol::account::AccountId;
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::note::{Note, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_protocol::utils::serde::Deserializable;
    use miden_standards::note::P2idNote;
    use rand::SeedableRng;
    use rand_chacha::ChaCha20Rng;
    use tonic::metadata::MetadataMap;

    use super::*;
    use crate::rng::draw_word;

    /// Builds a private P2ID note whose serial number derives from `seed`.
    fn private_note(seed: u32) -> Note {
        let sender = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let target = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let faucet = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let mut rng = ChaCha20Rng::seed_from_u64(u64::from(seed));

        P2idNote::builder()
            .sender(sender)
            .target(target)
            .asset(FungibleAsset::new(faucet, 100).unwrap())
            .note_type(NoteType::Private)
            .serial_number(draw_word(&mut rng))
            .build()
            .unwrap()
            .into()
    }

    fn fetched_note(header: &NoteHeader, details: NoteDetails) -> FetchedNote {
        FetchedNote {
            header: Some((*header).into()),
            details: Some(details.into()),
            committed_in_block: None,
        }
    }

    #[test]
    fn matching_note_decodes() {
        let note = private_note(1);
        let mut fetched = fetched_note(note.header(), NoteDetails::from(note.clone()));
        fetched.committed_in_block = Some(BlockNumber::from(9).into());

        let info = fetched.decode_and_verify().unwrap();

        assert_eq!(info.header, *note.header());
        assert_eq!(
            NoteDetails::read_from_bytes(&info.details_bytes).unwrap().commitment(),
            note.details_commitment()
        );
        assert_eq!(info.block_hint, Some(BlockNumber::from(9)));
    }

    #[test]
    fn mismatched_details_are_rejected() {
        let note_a = private_note(3);
        let note_b = private_note(4);
        assert_ne!(note_a.details_commitment(), note_b.details_commitment());

        let fetched = fetched_note(note_b.header(), NoteDetails::from(note_a.clone()));

        assert!(fetched.clone().decode_and_verify().is_err());
        let error = fetched.decode_fields().unwrap().verify().unwrap_err();
        assert_eq!(error.header, note_b.details_commitment());
        assert_eq!(error.details, note_a.details_commitment());
    }

    // SEND RETRY
    // --------------------------------------------------------------------------------------------

    fn status_failure(code: Code) -> SendFailure {
        SendFailure::Status(Status::new(code, "test failure"))
    }

    fn status_failure_with_retry_after(code: Code, seconds: &str) -> SendFailure {
        let mut metadata = MetadataMap::new();
        metadata.insert("retry-after", seconds.parse().unwrap());
        SendFailure::Status(Status::with_metadata(code, "test failure", metadata))
    }

    fn connect_failure() -> SendFailure {
        SendFailure::Connect(NoteTransportError::Network("connection refused".to_string()))
    }

    #[test]
    fn retry_delay_retries_only_transient_failures() {
        for failure in [
            connect_failure(),
            status_failure(Code::Unavailable),
            status_failure(Code::DeadlineExceeded),
            status_failure_with_retry_after(Code::ResourceExhausted, "2"),
        ] {
            assert!(retry_delay(&failure, 0, 250).is_some(), "{failure:?}");
        }

        // Without `retry-after`, `ResourceExhausted` means a note that is too large or a full
        // storage. A `retry-after` value does not make a permanent failure retryable.
        for failure in [
            status_failure(Code::ResourceExhausted),
            status_failure(Code::InvalidArgument),
            status_failure(Code::FailedPrecondition),
            status_failure(Code::Internal),
            status_failure(Code::Cancelled),
            status_failure_with_retry_after(Code::InvalidArgument, "1"),
        ] {
            assert_eq!(retry_delay(&failure, 0, 250), None, "{failure:?}");
        }
    }
}

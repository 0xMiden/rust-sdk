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
use tonic::{Code, Request};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;
#[cfg(target_arch = "wasm32")]
use {core::time::Duration, tonic_web_wasm_client::options::FetchOptions};
#[cfg(not(target_arch = "wasm32"))]
use {
    std::time::Duration,
    tonic::transport::{Channel, ClientTlsConfig},
};

use super::generated::note_transport::api_client::ApiClient;
use super::generated::note_transport::{
    FetchNotesCursor,
    FetchNotesRequest,
    FetchedNote,
    SendNoteRequest,
    SendNoteWithProofRequest,
    TransportNote as ProtoTransportNote,
};
use super::{NoteInfo, NoteTransportCursor, NoteTransportError, TransportNote};

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
        let block_hint = note
            .committed_in_block
            .or(note.after_block_num)
            .map(|block_num| BlockNumber::from(block_num.block_num));

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
        client: ApiClient::new(channel.clone()),
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
        client: ApiClient::new(wasm_client.clone()),
        health_client: HealthClient::new(wasm_client),
    })
}

/// Inner state holding the connected gRPC clients.
#[derive(Clone)]
struct ConnectedClient {
    client: ApiClient<Service>,
    health_client: HealthClient<Service>,
}

/// gRPC client for the note transport network.
///
/// The connection is established lazily on first use.
pub struct GrpcNoteTransportClient {
    inner: RwLock<Option<ConnectedClient>>,
    endpoint: String,
    timeout_ms: u64,
}

impl GrpcNoteTransportClient {
    /// Creates a new [`GrpcNoteTransportClient`] without establishing a connection. The connection
    /// will be established lazily on the first request.
    pub fn new(endpoint: String, timeout_ms: u64) -> Self {
        Self {
            inner: RwLock::new(None),
            endpoint,
            timeout_ms,
        }
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
    async fn api(&self) -> Result<ApiClient<Service>, NoteTransportError> {
        Ok(self.ensure_connected().await?.client)
    }

    /// Get a clone of the health client, connecting if needed.
    async fn health_api(&self) -> Result<HealthClient<Service>, NoteTransportError> {
        Ok(self.ensure_connected().await?.health_client)
    }

    /// Pushes a note to the note transport network.
    ///
    /// The note header and details use the node's typed Protobuf messages.
    pub async fn send_note(&self, note: TransportNote) -> Result<(), NoteTransportError> {
        self.send_note_inner(note, None).await
    }

    /// Pushes a note to the note transport network, relaying a block hint for the recipient.
    ///
    /// `block_hint` is forwarded as the request's `after_block_num`. It identifies the block from
    /// which the recipient should start scanning for the note's commitment.
    pub async fn send_note_with_block_hint(
        &self,
        note: TransportNote,
        block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_note_inner(note, Some(block_hint.as_u32())).await
    }

    /// Pushes a note to the note transport network together with its inclusion proof.
    ///
    /// The service verifies the proof against its node before it stores the note. It relays the
    /// commitment block to recipients as the exact inclusion block.
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

        self.api()
            .await?
            .send_note_with_proof(Request::new(request))
            .await
            .map_err(|e| {
                NoteTransportError::Network(format!("Send note with proof failed: {e:?}"))
            })?;

        Ok(())
    }

    /// Sends a note with an optional block hint.
    async fn send_note_inner(
        &self,
        note: TransportNote,
        after_block_num: Option<u32>,
    ) -> Result<(), NoteTransportError> {
        let request = SendNoteRequest {
            note: Some(proto_transport_note(note)),
            after_block_num: after_block_num.map(BlockNumber::from).map(Into::into),
        };

        self.api()
            .await?
            .send_note(Request::new(request))
            .await
            .map_err(|e| NoteTransportError::Network(format!("Send note failed: {e:?}")))?;

        Ok(())
    }

    /// Downloads notes for given tags from the note transport network.
    ///
    /// Returns notes labeled after the provided cursor (pagination), and an updated cursor.
    pub async fn fetch_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError> {
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
        Ok((notes, NoteTransportCursor::from_parts(cursor.nonce, cursor.sequence)))
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
    async fn send_note(&self, note: TransportNote) -> Result<(), NoteTransportError> {
        self.send_note(note).await
    }

    async fn send_note_with_block_hint(
        &self,
        note: TransportNote,
        block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_note_with_block_hint(note, block_hint).await
    }

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
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::crypto::rand::RandomCoin;
    use miden_protocol::note::{Note, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_protocol::utils::serde::Deserializable;
    use miden_standards::note::P2idNote;

    use super::*;

    /// Builds a private P2ID note whose serial number derives from `seed`.
    fn private_note(seed: u32) -> Note {
        let sender = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let target = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let faucet = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let mut rng = RandomCoin::new(Word::from(&[seed; 4]));

        P2idNote::builder()
            .sender(sender)
            .target(target)
            .asset(FungibleAsset::new(faucet, 100).unwrap())
            .note_type(NoteType::Private)
            .generate_serial_number(&mut rng)
            .build()
            .unwrap()
            .into()
    }

    fn fetched_note(header: &NoteHeader, details: NoteDetails) -> FetchedNote {
        FetchedNote {
            header: Some((*header).into()),
            details: Some(details.into()),
            after_block_num: None,
            committed_in_block: None,
        }
    }

    #[test]
    fn matching_note_decodes() {
        let note = private_note(1);
        let mut fetched = fetched_note(note.header(), NoteDetails::from(note.clone()));
        fetched.after_block_num = Some(BlockNumber::from(7).into());

        let info = fetched.decode_and_verify().unwrap();

        assert_eq!(info.header, *note.header());
        assert_eq!(
            NoteDetails::read_from_bytes(&info.details_bytes).unwrap().commitment(),
            note.details_commitment()
        );
        assert_eq!(info.block_hint, Some(BlockNumber::from(7)));
    }

    #[test]
    fn committed_block_takes_precedence_over_sender_hint() {
        let note = private_note(2);
        let mut fetched = fetched_note(note.header(), NoteDetails::from(note.clone()));
        fetched.after_block_num = Some(BlockNumber::from(7).into());
        fetched.committed_in_block = Some(BlockNumber::from(9).into());

        let info = fetched.decode_and_verify().unwrap();

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

    #[test]
    fn mismatched_transport_note_is_rejected() {
        let note_a = private_note(5);
        let note_b = private_note(6);

        let error =
            TransportNote::new(*note_b.header(), NoteDetails::from(note_a.clone())).unwrap_err();

        assert!(matches!(error, NoteTransportError::NoteDetailsMismatch { .. }));
    }

    #[test]
    fn transport_note_preserves_the_legacy_outbox_encoding() {
        let note = private_note(7);
        let header = *note.header();
        let details = NoteDetails::from(note.clone());
        let mut legacy = Vec::new();
        header.write_into(&mut legacy);
        details.to_bytes().write_into(&mut legacy);

        let transport_note = TransportNote::from(note);

        assert_eq!(transport_note.to_bytes(), legacy);
        assert_eq!(TransportNote::read_from_bytes(&legacy).unwrap(), transport_note);
    }

    #[test]
    fn missing_header_or_details_are_rejected() {
        let note = private_note(8);

        let mut without_header = fetched_note(note.header(), NoteDetails::from(note.clone()));
        without_header.header = None;
        let error = without_header.decode_and_verify().unwrap_err();
        assert!(error.to_string().contains("header"), "{error}");

        let mut without_details = fetched_note(note.header(), NoteDetails::from(note.clone()));
        without_details.details = None;
        let error = without_details.decode_and_verify().unwrap_err();
        assert!(error.to_string().contains("details"), "{error}");
    }
}

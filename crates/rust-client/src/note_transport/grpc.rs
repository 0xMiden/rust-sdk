//! gRPC-based note transport client.
//!
//! On native targets, the connection is established lazily on the first request using a
//! TLS-enabled `tonic` channel. On WASM, a `tonic_web_wasm_client` is created on demand.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::pin::Pin;
use core::task::{Context, Poll};

use futures::Stream;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteHeader, NoteTag};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_tx::utils::sync::RwLock;
use tonic::{Request, Streaming};
use tonic_health::pb::HealthCheckRequest;
use tonic_health::pb::health_client::HealthClient;
#[cfg(not(target_arch = "wasm32"))]
use {
    std::time::Duration,
    tonic::transport::{Channel, ClientTlsConfig},
};

use super::generated::miden_note_transport::miden_note_transport_client::MidenNoteTransportClient;
use super::generated::miden_note_transport::{
    FetchNotesRequest,
    SendNoteRequest,
    StreamNotesRequest,
    StreamNotesUpdate,
    TransportNote,
};
use super::{NoteInfo, NoteStream, NoteTransportCursor, NoteTransportError};

#[cfg(not(target_arch = "wasm32"))]
type Service = Channel;
#[cfg(target_arch = "wasm32")]
type Service = tonic_web_wasm_client::Client;

/// Establishes a connection to the note transport service, returning the gRPC clients.
#[cfg(not(target_arch = "wasm32"))]
async fn connect_channel(
    endpoint: &str,
    timeout_ms: u64,
) -> Result<(MidenNoteTransportClient<Service>, HealthClient<Service>), NoteTransportError> {
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
    Ok((MidenNoteTransportClient::new(channel.clone()), HealthClient::new(channel)))
}

/// Establishes a connection to the note transport service, returning the gRPC clients.
///
/// Note: `timeout_ms` is currently ignored on WASM as `tonic_web_wasm_client::Client` does not
/// support timeout configuration.
// TODO: refactor `connect_channel` so that WASM doesn't accept a timeout parameter.
#[cfg(target_arch = "wasm32")]
#[allow(clippy::unused_async)]
async fn connect_channel(
    endpoint: &str,
    _timeout_ms: u64,
) -> Result<(MidenNoteTransportClient<Service>, HealthClient<Service>), NoteTransportError> {
    let wasm_client = tonic_web_wasm_client::Client::new(String::from(endpoint));
    Ok((
        MidenNoteTransportClient::new(wasm_client.clone()),
        HealthClient::new(wasm_client),
    ))
}

/// Inner state holding the connected gRPC clients.
#[derive(Clone)]
struct ConnectedClient {
    client: MidenNoteTransportClient<Service>,
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
    /// Creates a new [`GrpcNoteTransportClient`] without establishing a connection.
    /// The connection will be established lazily on the first request.
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

        let (client, health_client) = connect_channel(&self.endpoint, self.timeout_ms).await?;
        let connected = ConnectedClient { client, health_client };
        *self.inner.write() = Some(connected.clone());
        Ok(connected)
    }

    /// Get a clone of the main client, connecting if needed.
    async fn api(&self) -> Result<MidenNoteTransportClient<Service>, NoteTransportError> {
        Ok(self.ensure_connected().await?.client)
    }

    /// Get a clone of the health client, connecting if needed.
    async fn health_api(&self) -> Result<HealthClient<Service>, NoteTransportError> {
        Ok(self.ensure_connected().await?.health_client)
    }

    /// Pushes a note to the note transport network.
    ///
    /// While the note header goes in plaintext, the provided note details can be encrypted.
    pub async fn send_note(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
    ) -> Result<(), NoteTransportError> {
        self.send_note_inner(header, details, None).await
    }

    /// Pushes a note to the note transport network, relaying a block hint for the recipient.
    ///
    /// `block_hint` is forwarded to the server (as the `TransportNote`'s `after_block_num`) as the
    /// block from which the recipient should start scanning for the note's commitment.
    pub async fn send_note_with_block_hint(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_note_inner(header, details, Some(block_hint.as_u32())).await
    }

    /// Sends a note, passing the optional block hint straight through to the wire `TransportNote`.
    async fn send_note_inner(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        after_block_num: Option<u32>,
    ) -> Result<(), NoteTransportError> {
        let request = SendNoteRequest {
            note: Some(TransportNote {
                header: header.to_bytes(),
                details,
                after_block_num,
            }),
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
        let request = FetchNotesRequest { tags: tags_int, cursor: cursor.value() };

        let response = self
            .api()
            .await?
            .fetch_notes(Request::new(request))
            .await
            .map_err(|e| NoteTransportError::Network(format!("Fetch notes failed: {e:?}")))?;

        let response = response.into_inner();

        // Convert protobuf notes to internal format and track the most recent received timestamp
        let mut notes = Vec::new();

        for pnote in response.notes {
            let header = NoteHeader::read_from_bytes(&pnote.header)?;

            notes.push(NoteInfo {
                header,
                details_bytes: pnote.details,
                block_hint: pnote.after_block_num.map(BlockNumber::from),
            });
        }

        Ok((notes, response.cursor.into()))
    }

    /// Stream notes from the note transport network.
    ///
    /// Subscribes to a given tag.
    /// New notes are received periodically.
    pub async fn stream_notes(
        &self,
        tag: NoteTag,
        cursor: NoteTransportCursor,
    ) -> Result<NoteStreamAdapter, NoteTransportError> {
        let request = StreamNotesRequest {
            tag: tag.as_u32(),
            cursor: cursor.value(),
        };

        let response = self
            .api()
            .await?
            .stream_notes(request)
            .await
            .map_err(|e| NoteTransportError::Network(format!("Stream notes failed: {e:?}")))?;
        Ok(NoteStreamAdapter::new(response.into_inner()))
    }

    /// gRPC-standardized server health-check.
    ///
    /// Checks if the note transport node and respective gRPC services are serving requests.
    /// Currently the grPC server operates only one service labelled `MidenNoteTransport`.
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
    async fn send_note(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
    ) -> Result<(), NoteTransportError> {
        self.send_note(header, details).await
    }

    async fn send_note_with_block_hint(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_note_with_block_hint(header, details, block_hint).await
    }

    async fn fetch_notes(
        &self,
        tags: &[NoteTag],
        cursor: NoteTransportCursor,
    ) -> Result<(Vec<NoteInfo>, NoteTransportCursor), NoteTransportError> {
        self.fetch_notes(tags, cursor).await
    }

    async fn stream_notes(
        &self,
        tag: NoteTag,
        cursor: NoteTransportCursor,
    ) -> Result<Box<dyn NoteStream>, NoteTransportError> {
        let stream = self.stream_notes(tag, cursor).await?;
        Ok(Box::new(stream))
    }
}

/// Convert from `tonic::Streaming<StreamNotesUpdate>` to [`NoteStream`]
pub struct NoteStreamAdapter {
    inner: Streaming<StreamNotesUpdate>,
}

impl NoteStreamAdapter {
    /// Create a new [`NoteStreamAdapter`]
    pub fn new(stream: Streaming<StreamNotesUpdate>) -> Self {
        Self { inner: stream }
    }
}

impl Stream for NoteStreamAdapter {
    type Item = Result<Vec<NoteInfo>, NoteTransportError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match Pin::new(&mut self.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(update))) => {
                // Convert StreamNotesUpdate to Vec<NoteInfo>
                let mut notes = Vec::new();
                for pnote in update.notes {
                    let header = NoteHeader::read_from_bytes(&pnote.header)?;

                    notes.push(NoteInfo {
                        header,
                        details_bytes: pnote.details,
                        block_hint: pnote.after_block_num.map(BlockNumber::from),
                    });
                }
                Poll::Ready(Some(Ok(notes)))
            },
            Poll::Ready(Some(Err(status))) => Poll::Ready(Some(Err(NoteTransportError::Network(
                format!("tonic status: {status}"),
            )))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl NoteStream for NoteStreamAdapter {}

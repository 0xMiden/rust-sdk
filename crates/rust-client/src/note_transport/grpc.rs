//! gRPC-based note transport client.
//!
//! On native targets, the connection is established lazily on the first request using a TLS-enabled
//! `tonic` channel. On WASM, a `tonic_web_wasm_client` is created on demand.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use miden_objects::DecodeMessageExt;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteDetails, NoteHeader, NoteTag};
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_tx::utils::sync::RwLock;
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
    SendNoteRequest,
    TransportNote,
};
use super::{NoteInfo, NoteTransportCursor, NoteTransportError};

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
    pub async fn send_note(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
    ) -> Result<(), NoteTransportError> {
        self.send_note_inner(header, details, None).await
    }

    /// Pushes a note to the note transport network, relaying a block hint for the recipient.
    ///
    /// `block_hint` is forwarded as the request's `after_block_num`. It identifies the block from
    /// which the recipient should start scanning for the note's commitment.
    pub async fn send_note_with_block_hint(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        block_hint: BlockNumber,
    ) -> Result<(), NoteTransportError> {
        self.send_note_inner(header, details, Some(block_hint.as_u32())).await
    }

    /// Sends a note with an optional block hint.
    async fn send_note_inner(
        &self,
        header: NoteHeader,
        details: Vec<u8>,
        after_block_num: Option<u32>,
    ) -> Result<(), NoteTransportError> {
        let details = NoteDetails::read_from_bytes(&details)?;
        let request = SendNoteRequest {
            note: Some(TransportNote {
                header: Some(header.into()),
                details: Some(details.into()),
            }),
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

        // Convert the Protobuf notes to the client format.
        let mut notes = Vec::new();

        for pnote in response.notes {
            let header: NoteHeader = pnote
                .header
                .ok_or_else(|| NoteTransportError::Network("fetched note has no header".into()))?
                .decode_and_verify()
                .map_err(|error| NoteTransportError::Network(error.to_string()))?;
            let details: NoteDetails = pnote
                .details
                .ok_or_else(|| NoteTransportError::Network("fetched note has no details".into()))?
                .decode_and_verify()
                .map_err(|error| NoteTransportError::Network(error.to_string()))?;
            let block_hint = pnote
                .committed_in_block
                .or(pnote.after_block_num)
                .map(|block_num| BlockNumber::from(block_num.block_num));

            notes.push(NoteInfo {
                header,
                details_bytes: details.to_bytes(),
                block_hint,
            });
        }

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
}

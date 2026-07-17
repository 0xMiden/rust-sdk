use alloc::string::String;
use core::fmt::Write;
use core::ops::{Deref, DerefMut};

use api_client_wrapper::{ApiClient, InnerClient};
use miden_protocol::Word;
use tonic::metadata::AsciiMetadataValue;
use tonic::metadata::errors::InvalidMetadataValue;
use tonic::service::Interceptor;

// WEB CLIENT
// ================================================================================================

#[cfg(target_arch = "wasm32")]
pub(crate) mod api_client_wrapper {
    use alloc::string::String;

    use miden_protocol::Word;
    use tonic::service::interceptor::InterceptedService;

    use super::{MetadataInterceptor, accept_header_interceptor};
    use crate::rpc::RpcError;
    use crate::rpc::generated::rpc::api_client::ApiClient as ProtoClient;

    pub type WasmClient = tonic_web_wasm_client::Client;
    pub type InnerClient = ProtoClient<InterceptedService<WasmClient, MetadataInterceptor>>;
    #[derive(Clone)]
    pub struct ApiClient {
        pub(crate) client: InnerClient,
        wasm_client: WasmClient,
        bearer_token: Option<String>,
        max_decoding_message_size: usize,
    }

    impl ApiClient {
        /// Connects to the Miden node API using the provided URL and genesis commitment.
        ///
        /// When `bearer_token` is `Some`, an `authorization: Bearer <token>` header is
        /// injected into every outbound request alongside the standard `accept` header.
        // Kept async for API parity with the native client; in WASM this is synchronous.
        #[allow(clippy::unused_async)]
        pub async fn new_client(
            endpoint: String,
            _timeout_ms: u64,
            genesis_commitment: Option<Word>,
            bearer_token: Option<String>,
            max_decoding_message_size: usize,
        ) -> Result<ApiClient, RpcError> {
            let wasm_client = WasmClient::new(endpoint);
            let interceptor =
                accept_header_interceptor(genesis_commitment, bearer_token.as_deref())?;
            let client = ProtoClient::with_interceptor(wasm_client.clone(), interceptor)
                .max_decoding_message_size(max_decoding_message_size);
            Ok(ApiClient {
                client,
                wasm_client,
                bearer_token,
                max_decoding_message_size,
            })
        }

        /// Connects to the Miden node API without injecting an Accept header.
        ///
        /// `bearer_token`, if set, is still forwarded as `authorization: Bearer <token>`.
        // Kept async for API parity with the native client; in WASM this is synchronous.
        #[allow(clippy::unused_async)]
        pub async fn new_client_without_accept_header(
            endpoint: String,
            _timeout_ms: u64,
            bearer_token: Option<String>,
            max_decoding_message_size: usize,
        ) -> Result<ApiClient, RpcError> {
            let wasm_client = WasmClient::new(endpoint);
            let interceptor =
                MetadataInterceptor::default().with_bearer_token(bearer_token.as_deref())?;
            let client = ProtoClient::with_interceptor(wasm_client.clone(), interceptor)
                .max_decoding_message_size(max_decoding_message_size);
            Ok(ApiClient {
                client,
                wasm_client,
                bearer_token,
                max_decoding_message_size,
            })
        }

        /// Returns a new `ApiClient` with an updated genesis commitment.
        /// This creates a new client that shares the same underlying channel. Any
        /// `bearer_token` passed to the constructor is preserved.
        pub fn set_genesis_commitment(&mut self, genesis_commitment: Word) -> &mut Self {
            // The bearer token was validated at construction time; re-applying the same
            // value here cannot fail.
            let interceptor =
                accept_header_interceptor(Some(genesis_commitment), self.bearer_token.as_deref())
                    .expect("bearer token already validated at construction time");
            self.client = ProtoClient::with_interceptor(self.wasm_client.clone(), interceptor)
                .max_decoding_message_size(self.max_decoding_message_size);
            self
        }
    }
}

// CLIENT
// ================================================================================================

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod api_client_wrapper {
    use alloc::boxed::Box;
    use alloc::string::String;
    use core::time::Duration;

    use miden_protocol::Word;
    use tonic::service::interceptor::InterceptedService;
    use tonic::transport::Channel;

    use super::{MetadataInterceptor, accept_header_interceptor};
    use crate::rpc::RpcError;
    use crate::rpc::generated::rpc::api_client::ApiClient as ProtoClient;

    pub type InnerClient = ProtoClient<InterceptedService<Channel, MetadataInterceptor>>;
    #[derive(Clone)]
    pub struct ApiClient {
        pub(crate) client: InnerClient,
        channel: Channel,
        bearer_token: Option<String>,
        max_decoding_message_size: usize,
    }

    impl ApiClient {
        /// Connects to the Miden node API using the provided URL, timeout and genesis commitment.
        ///
        /// When `bearer_token` is `Some`, an `authorization: Bearer <token>` header is
        /// injected into every outbound request alongside the standard `accept` header.
        pub async fn new_client(
            endpoint: String,
            timeout_ms: u64,
            genesis_commitment: Option<Word>,
            bearer_token: Option<String>,
            max_decoding_message_size: usize,
        ) -> Result<ApiClient, RpcError> {
            // Build the interceptor first so an invalid bearer token fails fast,
            // before we attempt the network connection.
            let interceptor =
                accept_header_interceptor(genesis_commitment, bearer_token.as_deref())?;

            // Setup connection channel.
            let endpoint = tonic::transport::Endpoint::try_from(endpoint)
                .map_err(|err| RpcError::ConnectionError(Box::new(err)))?
                .timeout(Duration::from_millis(timeout_ms));
            let channel = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .map_err(|err| RpcError::ConnectionError(Box::new(err)))?
                .connect()
                .await
                .map_err(|err| RpcError::ConnectionError(Box::new(err)))?;

            // Return the connected client.
            let client = ProtoClient::with_interceptor(channel.clone(), interceptor)
                .max_decoding_message_size(max_decoding_message_size);
            Ok(ApiClient {
                client,
                channel,
                bearer_token,
                max_decoding_message_size,
            })
        }

        /// Connects to the Miden node API without injecting an Accept header.
        ///
        /// `bearer_token`, if set, is still forwarded as `authorization: Bearer <token>`.
        pub async fn new_client_without_accept_header(
            endpoint: String,
            timeout_ms: u64,
            bearer_token: Option<String>,
            max_decoding_message_size: usize,
        ) -> Result<ApiClient, RpcError> {
            // Fail fast on an invalid bearer token, before opening the channel.
            let interceptor =
                MetadataInterceptor::default().with_bearer_token(bearer_token.as_deref())?;

            // Setup connection channel.
            let endpoint = tonic::transport::Endpoint::try_from(endpoint)
                .map_err(|err| RpcError::ConnectionError(Box::new(err)))?
                .timeout(Duration::from_millis(timeout_ms));
            let channel = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .map_err(|err| RpcError::ConnectionError(Box::new(err)))?
                .connect()
                .await
                .map_err(|err| RpcError::ConnectionError(Box::new(err)))?;

            let client = ProtoClient::with_interceptor(channel.clone(), interceptor)
                .max_decoding_message_size(max_decoding_message_size);
            Ok(ApiClient {
                client,
                channel,
                bearer_token,
                max_decoding_message_size,
            })
        }

        /// Returns a new `ApiClient` with an updated genesis commitment.
        /// This creates a new client that shares the same underlying channel. Any
        /// `bearer_token` passed to the constructor is preserved.
        pub fn set_genesis_commitment(&mut self, genesis_commitment: Word) -> &mut Self {
            // The bearer token was validated at construction time; re-applying the same
            // value here cannot fail.
            let interceptor =
                accept_header_interceptor(Some(genesis_commitment), self.bearer_token.as_deref())
                    .expect("bearer token already validated at construction time");
            self.client = ProtoClient::with_interceptor(self.channel.clone(), interceptor)
                .max_decoding_message_size(self.max_decoding_message_size);
            self
        }
    }
}

impl Deref for ApiClient {
    type Target = InnerClient;
    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl DerefMut for ApiClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

// INTERCEPTOR
// ================================================================================================

/// Interceptor designed to inject required metadata into all [`ApiClient`] requests.
#[derive(Default, Clone)]
pub struct MetadataInterceptor {
    metadata: alloc::collections::BTreeMap<&'static str, AsciiMetadataValue>,
}

impl MetadataInterceptor {
    /// Adds or overwrites metadata on the interceptor.
    pub fn with_metadata(
        mut self,
        key: &'static str,
        value: String,
    ) -> Result<Self, InvalidMetadataValue> {
        self.metadata.insert(key, AsciiMetadataValue::try_from(value)?);
        Ok(self)
    }

    /// Adds or overwrites the `authorization: Bearer <token>` header on the interceptor.
    /// A `None` token is a no-op.
    ///
    /// Returns [`RpcError::ConnectionError`] if the token is not a valid ASCII metadata
    /// value, mirroring the behaviour of other transport-setup failures on the client.
    pub(super) fn with_bearer_token(
        self,
        bearer_token: Option<&str>,
    ) -> Result<Self, crate::rpc::RpcError> {
        let Some(token) = bearer_token else {
            return Ok(self);
        };
        self.with_metadata("authorization", alloc::format!("Bearer {token}"))
            .map_err(|err| crate::rpc::RpcError::ConnectionError(alloc::boxed::Box::new(err)))
    }
}

impl Interceptor for MetadataInterceptor {
    fn call(&mut self, request: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
        let mut request = request;
        for (key, value) in &self.metadata {
            request.metadata_mut().insert(*key, value.clone());
        }
        Ok(request)
    }
}

/// Returns the HTTP header [`MetadataInterceptor`] that is expected by Miden RPC.
///
/// The interceptor sets the `accept` header to the Miden API version and optionally includes the
/// genesis commitment. When `bearer_token` is `Some`, an `authorization: Bearer <token>` header
/// is also attached.
fn accept_header_interceptor(
    genesis_digest: Option<Word>,
    bearer_token: Option<&str>,
) -> Result<MetadataInterceptor, crate::rpc::RpcError> {
    let version = env!("CARGO_PKG_VERSION");
    let mut accept_value = format!("application/vnd.miden; version={version}");
    if let Some(commitment) = genesis_digest {
        write!(accept_value, "; genesis={}", commitment.to_hex())
            .expect("valid hex representation of Word");
    }

    MetadataInterceptor::default()
        .with_metadata("accept", accept_value)
        .expect("valid key/value metadata for interceptor")
        .with_bearer_token(bearer_token)
}

#[cfg(test)]
mod tests {
    use tonic::Request;
    use tonic::service::Interceptor;

    use super::{MetadataInterceptor, accept_header_interceptor};

    #[test]
    fn interceptor_injects_bearer_token_onto_request() {
        // Build the same interceptor that the native/WASM clients would use, with a caller
        // bearer token in addition to the standard `accept`.
        let mut interceptor =
            accept_header_interceptor(None, Some("test-token")).expect("build interceptor");

        // Run it against a bare request to inspect what actually ends up on the wire.
        let request = interceptor.call(Request::new(())).expect("interceptor call succeeds");
        let metadata = request.metadata();

        let auth = metadata
            .get("authorization")
            .expect("authorization header must be present on outbound request");
        assert_eq!(auth.to_str().unwrap(), "Bearer test-token");

        // The standard accept header is still set alongside the caller's header.
        assert!(metadata.get("accept").is_some(), "accept header must still be present");
    }

    #[test]
    fn interceptor_omits_authorization_when_no_token_configured() {
        let mut interceptor = accept_header_interceptor(None, None).expect("build interceptor");

        let request = interceptor.call(Request::new(())).expect("interceptor call succeeds");
        let metadata = request.metadata();

        assert!(
            metadata.get("authorization").is_none(),
            "authorization must not leak when no token is configured",
        );
        assert!(metadata.get("accept").is_some(), "accept header must still be present");
    }

    #[test]
    fn with_bearer_token_rejects_invalid_ascii_values() {
        // Control characters are not valid ASCII metadata values; the builder must reject
        // them rather than silently dropping the header.
        match MetadataInterceptor::default().with_bearer_token(Some("bad\nvalue")) {
            Err(crate::rpc::RpcError::ConnectionError(_)) => {},
            Err(other) => panic!("expected ConnectionError, got {other:?}"),
            Ok(_) => panic!("expected invalid metadata value to error"),
        }
    }
}

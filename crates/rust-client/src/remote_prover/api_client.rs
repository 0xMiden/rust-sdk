use core::ops::{Deref, DerefMut};

pub(crate) use api_client_wrapper::{ApiClient, InnerClient};

// WEB CLIENT
// ================================================================================================

#[cfg(target_arch = "wasm32")]
pub(crate) mod api_client_wrapper {
    use alloc::string::String;
    use core::time::Duration;

    use crate::remote_prover::RemoteProverClientError;
    use crate::remote_prover::generated::api_client::ApiClient as ProtoClient;

    pub type InnerClient = ProtoClient<tonic_web_wasm_client::Client>;

    /// Wrapper around the generated prover API client that hides the transport type.
    #[derive(Clone)]
    pub struct ApiClient(pub(crate) InnerClient);

    impl ApiClient {
        /// Connects to the remote prover at the provided endpoint.
        // Kept async for API parity with the native client; in WASM this is synchronous.
        #[allow(clippy::unused_async)]
        pub async fn new_client(
            endpoint: String,
            timeout: Duration,
        ) -> Result<ApiClient, RemoteProverClientError> {
            let fetch_options =
                tonic_web_wasm_client::options::FetchOptions::new().timeout(timeout);
            let web_client =
                tonic_web_wasm_client::Client::new_with_options(endpoint, fetch_options);
            Ok(ApiClient(ProtoClient::new(web_client)))
        }
    }
}

// NATIVE CLIENT
// ================================================================================================

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod api_client_wrapper {
    use alloc::string::String;
    use core::time::Duration;

    use crate::remote_prover::RemoteProverClientError;
    use crate::remote_prover::generated::api_client::ApiClient as ProtoClient;

    pub type InnerClient = ProtoClient<tonic::transport::Channel>;

    /// Wrapper around the generated prover API client that hides the transport type.
    #[derive(Clone)]
    pub struct ApiClient(pub(crate) InnerClient);

    impl ApiClient {
        /// Connects to the remote prover at the provided endpoint.
        pub async fn new_client(
            endpoint: String,
            timeout: Duration,
        ) -> Result<ApiClient, RemoteProverClientError> {
            let endpoint = tonic::transport::Endpoint::try_from(endpoint)
                .map_err(|err| RemoteProverClientError::ConnectionFailed(err.into()))?
                .timeout(timeout);
            let channel = endpoint
                .tls_config(tonic::transport::ClientTlsConfig::new().with_native_roots())
                .map_err(|err| RemoteProverClientError::ConnectionFailed(err.into()))?
                .connect()
                .await
                .map_err(|err| RemoteProverClientError::ConnectionFailed(err.into()))?;
            Ok(ApiClient(ProtoClient::new(channel)))
        }
    }
}

impl Deref for ApiClient {
    type Target = InnerClient;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for ApiClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

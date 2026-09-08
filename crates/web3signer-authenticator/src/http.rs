//! HTTP transport for a `Web3Signer` instance, backed by `reqwest`.

use alloc::format;
use alloc::string::{String, ToString};
use core::time::Duration;

use miden_protocol::vm::FutureMaybeSend;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Client, RequestBuilder};

use crate::{SignerTransport, Web3SignerError};

/// Time a single request to the web3 signer is given before it is abandoned.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A [`SignerTransport`] that holds [`reqwest::Client`] as the internal HTTP client.
#[derive(Clone, Debug)]
pub struct HttpTransport {
    client: Client,
    base_url: String,
}

impl HttpTransport {
    /// Builds a transport for the `Web3Signer` instance at `url`.
    ///
    /// # Errors
    /// Returns an error if the HTTP client cannot be built.
    pub fn new(url: &str) -> Result<Self, Web3SignerError> {
        let client = Client::builder().timeout(REQUEST_TIMEOUT).build().map_err(|err| {
            Web3SignerError::Transport {
                path: url.to_string(),
                message: err.to_string(),
            }
        })?;

        Ok(Self {
            client,
            base_url: url.trim_end_matches('/').to_string(),
        })
    }

    /// Sends a prepared request and returns its body, mapping a non-success status to an error.
    async fn send(&self, path: &str, request: RequestBuilder) -> Result<String, Web3SignerError> {
        let transport_error = |err: reqwest::Error| Web3SignerError::Transport {
            path: path.to_string(),
            message: err.to_string(),
        };

        let response = request.send().await.map_err(transport_error)?;
        let status = response.status();
        let body = response.text().await.map_err(transport_error)?;

        if !status.is_success() {
            return Err(Web3SignerError::Transport {
                path: path.to_string(),
                message: format!("signer answered {status}: {}", body.trim()),
            });
        }

        Ok(body)
    }
}

impl SignerTransport for HttpTransport {
    fn get(&self, path: &str) -> impl FutureMaybeSend<Result<String, Web3SignerError>> {
        let request = self
            .client
            .get(format!("{}{path}", self.base_url))
            .header(ACCEPT, "application/json");

        self.send(path, request)
    }

    fn post(
        &self,
        path: &str,
        body: String,
    ) -> impl FutureMaybeSend<Result<String, Web3SignerError>> {
        // The signing endpoint answers with the signature as text unless asked for JSON.
        let request = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header(ACCEPT, "text/plain")
            .header(CONTENT_TYPE, "application/json")
            .body(body);

        self.send(path, request)
    }
}

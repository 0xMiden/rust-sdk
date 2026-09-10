//! HTTP transport for a `Web3Signer` instance, backed by `reqwest`.

use alloc::format;
use alloc::string::{String, ToString};
use core::time::Duration;

use miden_protocol::vm::FutureMaybeSend;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{Client, RequestBuilder, Url};

use crate::{SignerTransport, Web3SignerError};

/// Time a single request to the web3 signer is given before it is abandoned.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// A [`SignerTransport`] that holds [`reqwest::Client`] as the internal HTTP client.
#[derive(Clone, Debug)]
pub struct HttpTransport {
    client: Client,
    base_url: Url,
}

impl HttpTransport {
    /// Builds a transport for the `Web3Signer` instance at `url`.
    ///
    /// # Errors
    /// Returns an error if `url` is not a valid `http` or `https` URL, or if the HTTP client
    /// cannot be built.
    pub fn new(url: &str) -> Result<Self, Web3SignerError> {
        let base_url = Url::parse(url).map_err(|err| Web3SignerError::InvalidUrl {
            url: url.to_string(),
            message: err.to_string(),
        })?;

        if !matches!(base_url.scheme(), "http" | "https") {
            return Err(Web3SignerError::InvalidUrl {
                url: url.to_string(),
                message: format!("expected an `http` or `https` URL, got `{}`", base_url.scheme()),
            });
        }

        let client = Client::builder().timeout(REQUEST_TIMEOUT).build().map_err(|err| {
            Web3SignerError::Transport {
                url: base_url.to_string(),
                message: err.to_string(),
            }
        })?;

        Ok(Self { client, base_url })
    }

    /// Resolves an absolute request `path` against the signer's base URL, replacing any path the
    /// base URL carries.
    fn request_url(&self, path: &str) -> Result<Url, Web3SignerError> {
        self.base_url.join(path).map_err(|err| Web3SignerError::InvalidUrl {
            url: format!("{}{path}", self.base_url),
            message: err.to_string(),
        })
    }

    /// Sends a prepared request and returns its body, mapping a non-success status to an error.
    async fn send(&self, request: RequestBuilder) -> Result<String, Web3SignerError> {
        let request = request.build().map_err(|err| Web3SignerError::Transport {
            url: self.base_url.to_string(),
            message: err.to_string(),
        })?;
        let url = request.url().to_string();

        let transport_error = |err: reqwest::Error| Web3SignerError::Transport {
            url: url.clone(),
            message: err.to_string(),
        };

        let response = self.client.execute(request).await.map_err(transport_error)?;
        let status = response.status();
        let body = response.text().await.map_err(transport_error)?;

        if !status.is_success() {
            return Err(Web3SignerError::Transport {
                url,
                message: format!("signer answered {status}: {}", body.trim()),
            });
        }

        Ok(body)
    }
}

impl SignerTransport for HttpTransport {
    fn get(&self, path: &str) -> impl FutureMaybeSend<Result<String, Web3SignerError>> {
        async move {
            let request =
                self.client.get(self.request_url(path)?).header(ACCEPT, "application/json");

            self.send(request).await
        }
    }

    fn post(
        &self,
        path: &str,
        body: String,
    ) -> impl FutureMaybeSend<Result<String, Web3SignerError>> {
        async move {
            // The signing endpoint answers with the signature as text unless asked for JSON.
            let request = self
                .client
                .post(self.request_url(path)?)
                .header(ACCEPT, "text/plain")
                .header(CONTENT_TYPE, "application/json")
                .body(body);

            self.send(request).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PUBLIC_KEYS_PATH;

    #[test]
    fn invalid_urls_are_rejected() {
        for url in ["", "127.0.0.1:9000", "ftp://127.0.0.1:9000", "http://"] {
            assert!(
                matches!(HttpTransport::new(url), Err(Web3SignerError::InvalidUrl { .. })),
                "`{url}` should be rejected"
            );
        }
    }

    #[test]
    fn a_request_path_is_appended_to_the_base_url() {
        for base in ["http://127.0.0.1:9000", "http://127.0.0.1:9000/"] {
            let transport = HttpTransport::new(base).expect("base URL is valid");
            let url = transport.request_url(PUBLIC_KEYS_PATH).expect("path is valid");

            assert_eq!(url.as_str(), "http://127.0.0.1:9000/api/v1/eth1/publicKeys");
        }
    }
}

use alloc::string::String;

use miden_protocol::vm::FutureMaybeSend;

use crate::Web3SignerError;

/// Request transport for a `Web3Signer` instance.
///
/// An implementation owns the web3 signer's base URL, resolves the given absolute `path` against
/// it, and maps any non-success response status to [`Web3SignerError::Transport`]. On success, both
/// methods return the response body.
///
/// [`crate::http::HttpTransport`] implements this over `reqwest` and requires the `std` feature.
/// Targets without it, such as wasm32, provide their own.
pub trait SignerTransport: Send + Sync {
    /// Sends a GET request to `path` on the signer's base URL.
    fn get(&self, path: &str) -> impl FutureMaybeSend<Result<String, Web3SignerError>>;

    /// Sends a POST request to `path` on the signer's base URL, with a JSON `body`.
    fn post(
        &self,
        path: &str,
        body: String,
    ) -> impl FutureMaybeSend<Result<String, Web3SignerError>>;
}

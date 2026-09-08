use alloc::string::String;

use miden_protocol::vm::FutureMaybeSend;

use crate::Web3SignerError;

/// Request transport for a `Web3Signer` instance.
///
/// An implementation owns the signer's base URL, prepends it to the `path` it is given, and maps
/// any non-success response status to [`Web3SignerError::Transport`] instead of returning the
/// body. Both methods return the response body.
///
/// `HttpTransport` implements this over `reqwest` and requires the `std` feature. Targets
/// without it, such as wasm32 or embedded ones, provide their own.
pub trait SignerTransport: Send + Sync {
    /// Sends a GET request to `path`.
    fn get(&self, path: &str) -> impl FutureMaybeSend<Result<String, Web3SignerError>>;

    /// Sends a POST request to `path` with a JSON `body`.
    fn post(
        &self,
        path: &str,
        body: String,
    ) -> impl FutureMaybeSend<Result<String, Web3SignerError>>;
}

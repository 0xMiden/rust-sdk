use core::time::Duration;

use tonic::Status;
use tracing::warn;

use crate::rpc::RpcEndpoint;

// CONSTS
// ================================================================================================

/// Default maximum number of retry attempts for rate-limited requests.
pub(super) const DEFAULT_MAX_RETRIES: u32 = 4;

/// Default fallback delay (in milliseconds) when no `retry-after` header is present.
pub(super) const DEFAULT_RETRY_INTERVAL_MS: u64 = 100;

// RETRY STATE
// ================================================================================================

/// Tracks retry attempts for a single RPC call and applies the node-provided cooldown policy.
///
/// The state is intentionally tiny: it counts how many retries have already been attempted and
/// keeps the endpoint the call targets, which decides whether an ambiguous failure may be repeated
/// at all. Delay selection is derived from the current gRPC [`Status`], preferring a non-zero
/// `retry-after` response metadata value when present and falling back to the configured retry
/// interval otherwise.
pub(super) struct RetryState {
    endpoint: RpcEndpoint,
    attempt: u32,
    max_retries: u32,
    retry_interval_ms: u64,
}

impl RetryState {
    /// Creates a new retry state for a fresh RPC call.
    pub(super) const fn new(
        endpoint: RpcEndpoint,
        max_retries: u32,
        retry_interval_ms: u64,
    ) -> Self {
        Self {
            endpoint,
            attempt: 0,
            max_retries,
            retry_interval_ms,
        }
    }

    /// Applies retry policy for the provided status.
    ///
    /// Returns `true` after waiting the requested cooldown when the error is retryable and the
    /// attempt limit has not been reached. Returns `false` for non-retryable statuses or once the
    /// retry budget is exhausted.
    pub(super) async fn should_retry(&mut self, status: &Status) -> bool {
        if self.attempt >= self.max_retries || !is_retryable(self.endpoint, status) {
            return false;
        }

        let delay = retry_delay(status, self.retry_interval_ms);

        warn!(
            endpoint = %self.endpoint,
            code = %status.code(),
            attempt = self.attempt + 1,
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "retryable error from node, retrying after delay",
        );

        async_sleep(delay).await;
        self.attempt += 1;
        true
    }
}

// HELPERS
// ================================================================================================

/// Returns whether the call may be repeated after the provided status.
///
/// Each code is listed on its own arm rather than filtering a shared retryable set, so a code added
/// here later has to be classified against non-idempotent endpoints explicitly instead of
/// inheriting the read policy.
fn is_retryable(endpoint: RpcEndpoint, status: &Status) -> bool {
    match status.code() {
        // Rate limiting is a rejection issued before the request is processed, so repeating it
        // cannot duplicate work.
        tonic::Code::ResourceExhausted => true,
        // This code carries no evidence about whether the node processed the request: it is usually
        // produced by the local gRPC stack when the connection breaks, so the response may simply
        // have been lost on the way back.
        tonic::Code::Unavailable => endpoint.is_idempotent(),
        _ => false,
    }
}

fn retry_delay(status: &Status, fallback_ms: u64) -> Duration {
    extract_retry_after(status)
        .filter(|delay| !delay.is_zero())
        .unwrap_or(Duration::from_millis(fallback_ms))
}

fn extract_retry_after(status: &Status) -> Option<Duration> {
    status
        .metadata()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(not(target_arch = "wasm32"))]
async fn async_sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// On WASM, sleep using browser timers so retry delays are honored.
#[cfg(target_arch = "wasm32")]
async fn async_sleep(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use tonic::metadata::MetadataMap;
    use tonic::{Code, Status};

    use super::{DEFAULT_RETRY_INTERVAL_MS, RpcEndpoint, is_retryable, retry_delay};

    fn status_with_retry_after(retry_after: &str) -> Status {
        let mut metadata = MetadataMap::new();
        metadata.insert("retry-after", retry_after.parse().unwrap());
        Status::with_metadata(Code::ResourceExhausted, "Too Many Requests! Wait for 0s", metadata)
    }

    /// A submission whose response was lost may already have been accepted, so repeating it would
    /// surface the resulting conflict instead of the original success.
    #[test]
    fn submissions_do_not_retry_unavailable() {
        for endpoint in [RpcEndpoint::SubmitProvenTx, RpcEndpoint::SubmitProvenBatch] {
            assert!(!is_retryable(endpoint, &Status::new(Code::Unavailable, "transport error")));
        }
    }

    #[test]
    fn submissions_retry_resource_exhausted() {
        for endpoint in [RpcEndpoint::SubmitProvenTx, RpcEndpoint::SubmitProvenBatch] {
            assert!(is_retryable(endpoint, &Status::new(Code::ResourceExhausted, "rate limited")));
        }
    }

    #[test]
    fn reads_retry_both_transient_codes() {
        let endpoint = RpcEndpoint::GetBlockHeaderByNumber;

        assert!(is_retryable(endpoint, &Status::new(Code::Unavailable, "transport error")));
        assert!(is_retryable(endpoint, &Status::new(Code::ResourceExhausted, "rate limited")));
    }

    #[test]
    fn other_codes_are_never_retried() {
        for endpoint in [RpcEndpoint::SubmitProvenTx, RpcEndpoint::GetBlockHeaderByNumber] {
            assert!(!is_retryable(endpoint, &Status::new(Code::Internal, "node error")));
        }
    }

    #[test]
    fn zero_retry_after_uses_fallback_delay() {
        assert_eq!(
            retry_delay(&status_with_retry_after("0"), DEFAULT_RETRY_INTERVAL_MS),
            Duration::from_millis(DEFAULT_RETRY_INTERVAL_MS)
        );
    }
}

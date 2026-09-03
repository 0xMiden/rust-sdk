use core::time::Duration;

use tonic::Status;
use tracing::warn;

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
/// The state is intentionally tiny: it only counts how many retries have already been attempted.
/// Delay selection is derived from the current gRPC [`Status`], preferring a non-zero `retry-after`
/// response metadata value when present and falling back to the configured retry interval
/// otherwise.
pub(super) struct RetryState {
    attempt: u32,
    max_retries: u32,
    retry_interval_ms: u64,
}

impl RetryState {
    /// Creates a new retry state for a fresh RPC call.
    pub(super) const fn new(max_retries: u32, retry_interval_ms: u64) -> Self {
        Self {
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
        if self.attempt >= self.max_retries || !is_retryable(status) {
            return false;
        }

        let delay = retry_delay(status, self.retry_interval_ms);

        warn!(
            attempt = self.attempt + 1,
            delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            "rate-limited by node, retrying after delay",
        );

        async_sleep(delay).await;
        self.attempt += 1;
        true
    }
}

// HELPERS
// ================================================================================================

fn is_retryable(status: &Status) -> bool {
    matches!(status.code(), tonic::Code::ResourceExhausted | tonic::Code::Unavailable)
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

    use super::{DEFAULT_RETRY_INTERVAL_MS, retry_delay};

    fn status_with_retry_after(retry_after: &str) -> Status {
        let mut metadata = MetadataMap::new();
        metadata.insert("retry-after", retry_after.parse().unwrap());
        Status::with_metadata(Code::ResourceExhausted, "Too Many Requests! Wait for 0s", metadata)
    }

    #[test]
    fn zero_retry_after_uses_fallback_delay() {
        assert_eq!(
            retry_delay(&status_with_retry_after("0"), DEFAULT_RETRY_INTERVAL_MS),
            Duration::from_millis(DEFAULT_RETRY_INTERVAL_MS)
        );
    }
}

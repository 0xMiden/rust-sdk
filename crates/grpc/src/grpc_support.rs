use std::time::Duration;

// RETRY HELPERS
// ================================================================================================

/// Returns the delay that the `retry-after` metadata value of `status` requests, in whole seconds.
pub(crate) fn extract_retry_after(status: &tonic::Status) -> Option<Duration> {
    status
        .metadata()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn async_sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

/// On WASM, sleep using browser timers so retry delays are honored.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn async_sleep(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

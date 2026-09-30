//! Invitation codes the account allowlist tests register accounts with.
//!
//! An invitation code binds to the first account that presents it and cannot be reused, so every
//! test needs a code of its own. Each call to [`create_invitation_code`] creates one on the node
//! through the sequencer administration API, which `scripts/start-test-node.sh` binds when it is
//! started with `MIDEN_ACCOUNT_ALLOWLIST=1`. Creating the code on demand also keeps a retried test
//! correct, because the retry creates a fresh code instead of reusing the one its previous attempt
//! consumed.

use std::fmt::Write;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use uuid::Uuid;

// CONSTANTS
// ================================================================================================

/// Env var naming the sequencer administration API, for example `http://127.0.0.1:50100`.
pub const ADMIN_API_ENV: &str = "MIDEN_NODE_ADMIN_URL";

/// How many times a request waits for the administration API to accept connections. The node serves
/// it from its own task, which can bind after the RPC does.
const CONNECT_ATTEMPTS: usize = 10;

/// How long a request waits between connection attempts.
const CONNECT_RETRY_DELAY: Duration = Duration::from_millis(500);

// INVITATION CODES
// ================================================================================================

/// Creates an invitation code that is bound to no account and returns its plaintext.
///
/// Fails when [`ADMIN_API_ENV`] is unset or the administration API refuses the request, because a
/// test that needs a code cannot run against a node that was started without allowlist enforcement.
pub async fn create_invitation_code() -> Result<String> {
    let admin_url = std::env::var(ADMIN_API_ENV).with_context(|| {
        format!("{ADMIN_API_ENV} is not set; start the node with MIDEN_ACCOUNT_ALLOWLIST=1")
    })?;

    // The code is unique per call, so no two tests can hold the same one.
    let code = format!("miden-client-test-invitation-{}", Uuid::new_v4());
    let url = format!(
        "{}/admin/allowlist/invitations/{}",
        admin_url.trim_end_matches('/'),
        digest_of(&code)
    );

    // The node stores the digest alone and binds the code to the first account that registers with
    // it, so the entry is created without an account.
    let request = serde_json::json!({ "account_id": null });
    let http = reqwest::Client::new();

    let mut attempt = 1;
    let response = loop {
        match http.put(&url).json(&request).send().await {
            Ok(response) => break response,
            Err(error) if error.is_connect() && attempt < CONNECT_ATTEMPTS => {
                tokio::time::sleep(CONNECT_RETRY_DELAY).await;
                attempt += 1;
            },
            Err(error) => {
                return Err(anyhow::Error::new(error).context(format!(
                    "failed to reach the node administration API at {url}; start the node with \
                     MIDEN_ACCOUNT_ALLOWLIST=1"
                )));
            },
        }
    };

    let status = response.status();
    ensure!(
        status.is_success(),
        "the node refused to create an invitation code: {status} {}",
        response.text().await.unwrap_or_default()
    );

    Ok(code)
}

/// Returns the lowercase hex SHA-256 of `code`. The node stores an invitation code as its digest
/// and takes the digest in the request path.
fn digest_of(code: &str) -> String {
    let mut digest = String::with_capacity(64);
    for byte in Sha256::digest(code.as_bytes()) {
        write!(digest, "{byte:02x}").expect("writing to a string never fails");
    }

    digest
}

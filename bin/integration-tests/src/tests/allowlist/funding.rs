//! Funding for the accounts the allowlist tests do not register.
//!
//! A registration makes the node pay the account through its funding service. An unregistered
//! account also needs the native asset, because the client executes a deploy before it asks the
//! node whether the account is allowlisted, and on a fee-charging chain the execution fails when
//! the vault cannot pay the fee. The tests pay such an account with a direct request to the same
//! service, which `scripts/start-test-node.sh` starts when it is started with
//! `MIDEN_ACCOUNT_ALLOWLIST=1`. The account then receives the same public note a registration
//! gives, so every account in these tests deploys the same way.

use anyhow::{Context, Result, ensure};
use miden_client::account::Account;

use crate::fee_funding::FUNDING_AMOUNT;

// CONSTANTS
// ================================================================================================

/// Env var naming the funding service, for example `http://127.0.0.1:50401`.
pub const FUNDING_SERVICE_ENV: &str = "MIDEN_FUNDING_SERVICE_URL";

// FUNDING
// ================================================================================================

/// Asks the funding service to pay [`FUNDING_AMOUNT`] of the native asset to `account`.
///
/// The service answers only once the note is committed, so a sync after this call finds the note.
/// Fails when [`FUNDING_SERVICE_ENV`] is unset or the service refuses the request.
pub async fn request_funds(account: &Account) -> Result<()> {
    let service_url = std::env::var(FUNDING_SERVICE_ENV).with_context(|| {
        format!("{FUNDING_SERVICE_ENV} is not set. Start the node with MIDEN_ACCOUNT_ALLOWLIST=1")
    })?;
    let url = format!("{}/request-funds", service_url.trim_end_matches('/'));

    let request = serde_json::json!({
        "account_id": account.id().to_hex(),
        "amount": FUNDING_AMOUNT,
    });

    let response = reqwest::Client::new()
        .post(&url)
        .json(&request)
        .send()
        .await
        .with_context(|| format!("failed to reach the funding service at {url}"))?;

    let status = response.status();
    ensure!(
        status.is_success(),
        "the funding service refused to fund account {}: {status} {}",
        account.id(),
        response.text().await.unwrap_or_default()
    );

    Ok(())
}

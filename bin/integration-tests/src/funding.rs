//! Draws the native fee asset from the node's funding service, which owns one account and hands out
//! pay-to-ID notes over an HTTP API. It is the only source of that asset the suite has.
//!
//! The service gathers every request that reaches it inside a short window into one transaction. A
//! caller therefore asks for all of its accounts at once, so that they share that transaction, and
//! so do the accounts of every other test process funding at the same moment.
//!
//! The service answers with the note before it builds the transaction which creates the note. Every
//! funding note is consumed as an unauthenticated input, so no test waits for the funding
//! transaction to commit. A test can submit its own transaction before the funding transaction
//! reaches the node. The test client's RPC layer resubmits such a rejected transaction (see
//! [`crate::submit_retry`]).

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use miden_client::account::AccountId;
use miden_client::auth::TransactionAuthenticator;
use miden_client::block::BlockNumber;
use miden_client::note::Note;
use miden_client::testing::fee::FeeFunder;
use miden_client::{Client, Deserializable};
use serde::{Deserialize, Serialize};
use tracing::warn;

// CONSTANTS
// ================================================================================================

/// Env var naming the funding service's base URL, mirroring the `--funding-service` argument.
pub const FUNDING_SERVICE_ENV: &str = "MIDEN_FUNDING_SERVICE_URL";

/// Amount of the native fee asset, in base units, each funded account receives. A fee runs a few
/// tens of thousands of base units, so this covers far more than any one test spends.
pub const FUNDING_AMOUNT: u64 = 10_000_000;

/// How long one `fund` call may take, with every attempt included. The service answers as soon as
/// it queues the note, so this bound is reached only when the service stops answering.
///
/// Keep this below the time after which nextest kills a test (`slow-timeout` in
/// `.config/nextest.toml`, 360s). A killed test reports only that it timed out. A test which
/// reaches this deadline fails with the funding error instead, which shows that the funding service
/// stopped answering.
const FUNDING_DEADLINE: Duration = Duration::from_secs(240);

/// How long one attempt may take. An attempt cannot outlive the whole `fund` call.
///
/// A caller that gives up is dropped from the service's next batch without an error from the
/// service, so the account is not funded. `fund` then returns the timeout as its error.
const REQUEST_TIMEOUT: Duration = FUNDING_DEADLINE;

/// How many times one funding request is sent before the run gives up.
///
/// Bounded rather than open-ended: a funding account whose on-chain code does not match the account
/// file the service was started with answers every request with the same retryable status forever,
/// and an unbounded retry would hang the suite instead of failing with a readable error.
const MAX_ATTEMPTS: u32 = 5;

/// How long to wait before the second attempt. Each further attempt doubles it.
const RETRY_BASE_DELAY: Duration = Duration::from_secs(1);

/// Upper bound on the wait between attempts.
const RETRY_MAX_DELAY: Duration = Duration::from_secs(15);

// WIRE TYPES
// ================================================================================================

/// The body of a funding request.
///
/// Declared here because the service's own types live in a private module of the node's binary
/// crate and are not reachable from outside it.
#[derive(Debug, Serialize)]
struct RequestFundsRequest {
    /// The account the note targets, in hexadecimal.
    account_id: String,
    /// The amount of the native asset, in base units.
    amount: u64,
}

/// The body of a successful funding response. The note is what the funded account consumes.
#[derive(Debug, Deserialize)]
struct RequestFundsResponse {
    /// The serialized note, in hexadecimal.
    note: String,
}

/// The body of a failed funding request.
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    /// The reason the request failed.
    error: String,
}

// FAILURE CLASSIFICATION
// ================================================================================================

/// Why one attempt at a funding request did not produce a note.
struct Failure {
    /// Whether sending the same request again may succeed.
    retryable: bool,
    error: anyhow::Error,
}

impl Failure {
    fn retryable(error: anyhow::Error) -> Self {
        Self { retryable: true, error }
    }

    fn fatal(error: anyhow::Error) -> Self {
        Self { retryable: false, error }
    }

    /// Classifies an answer by its status code.
    ///
    /// The service documents which codes leave its state untouched. A request that failed with 400,
    /// 409, 412 or 429 created no note. A request that failed with 408, 500 or 503 may still have
    /// created one, because the service can lose contact with the node after it submits the
    /// transaction, so a retry of those may fund the account twice. That is accepted here: a test
    /// account holding two funding notes still behaves, and the alternative is a flaky suite.
    fn from_status(status: u16, message: String) -> Self {
        let err = anyhow!("the funding service answered {status}: {message}");

        match status {
            // The funding account cannot cover the request. No retry will refill it.
            412 => Self::fatal(err.context(
                "the funding service's account is out of the native asset, so the chain it funds \
                 has to be given a funding account with a larger genesis balance",
            )),
            // The request itself is wrong, so it will be rejected the same way every time.
            400 => Self::fatal(err),
            _ => Self::retryable(err),
        }
    }
}

// FUNDING SERVICE FUNDER
// ================================================================================================

/// Funds accounts by asking the node's funding service for a note per account.
#[derive(Debug)]
pub struct FundingServiceFunder {
    /// The base URL the service listens on, without a trailing separator.
    base_url: String,
    http: reqwest::Client,
    /// Amount of the native fee asset, in base units, each funded account receives.
    amount: u64,
}

impl FundingServiceFunder {
    /// Builds a funder talking to the service at `base_url`.
    pub fn new(base_url: &str, amount: u64) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build the funding service HTTP client")?;

        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            http,
            amount,
        })
    }

    /// Asks for one note, retrying an attempt that left the service's state untouched.
    async fn request_note(&self, target: AccountId) -> Result<(AccountId, Note)> {
        let body = RequestFundsRequest {
            account_id: target.to_hex(),
            amount: self.amount,
        };

        let mut attempt = 1;
        loop {
            let Failure { retryable, error } = match self.post(&body).await {
                Ok(note) => return Ok((target, note)),
                Err(failure) => failure,
            };

            if !retryable || attempt == MAX_ATTEMPTS {
                return Err(error.context(format!(
                    "the funding service failed to fund {target} after {attempt} attempt(s)"
                )));
            }

            let delay = RETRY_BASE_DELAY.saturating_mul(1 << (attempt - 1)).min(RETRY_MAX_DELAY);
            warn!(
                target_id = %target,
                attempt,
                %error,
                "The funding service request failed, retrying",
            );
            tokio::time::sleep(delay).await;
            attempt += 1;
        }
    }

    /// Sends one request and decodes its answer.
    async fn post(&self, body: &RequestFundsRequest) -> Result<Note, Failure> {
        let url = format!("{}/request-funds", self.base_url);

        // A transport error covers both a service that is not up yet and one that dropped the
        // connection mid-request, and neither says whether a note was created, so it is treated
        // like the statuses that carry the same doubt.
        let response = self.http.post(&url).json(body).send().await.map_err(|err| {
            Failure::retryable(
                anyhow::Error::new(err)
                    .context(format!("the request to the funding service at {url} failed")),
            )
        })?;

        let status = response.status();
        let payload = response.bytes().await.map_err(|err| {
            Failure::retryable(
                anyhow::Error::new(err).context("failed to read the funding service's answer"),
            )
        })?;

        if !status.is_success() {
            // The service reports the reason in a JSON body. A body in any other shape, from a
            // proxy in front of it for instance, is reported as it is.
            let message = serde_json::from_slice::<ErrorResponse>(&payload)
                .map(|body| body.error)
                .unwrap_or_else(|_| String::from_utf8_lossy(&payload).trim().to_string());

            return Err(Failure::from_status(status.as_u16(), message));
        }

        // Past this point the service has created the note, so a failure to read it is not
        // something a retry fixes.
        decode_note(&payload).map_err(Failure::fatal)
    }
}

/// Reads the note out of a successful answer.
fn decode_note(payload: &[u8]) -> Result<Note> {
    let response: RequestFundsResponse =
        serde_json::from_slice(payload).context("failed to parse the funding service's answer")?;

    let bytes = hex::decode(&response.note)
        .context("the funding service returned a note that is not hexadecimal")?;

    Note::read_from_bytes(&bytes)
        .map_err(|err| anyhow!("failed to deserialize the funding service's note: {err}"))
}

#[async_trait::async_trait(?Send)]
impl FeeFunder for FundingServiceFunder {
    async fn fund(&self, account_ids: &[AccountId]) -> Result<Vec<(AccountId, Note)>> {
        if account_ids.is_empty() {
            return Ok(Vec::new());
        }

        // Sent together rather than one after another, so the notes are queued together and the
        // service can put them into one transaction.
        let requests = account_ids.iter().map(|target| self.request_note(*target));

        tokio::time::timeout(FUNDING_DEADLINE, futures::future::try_join_all(requests))
            .await
            .map_err(|_| {
                anyhow!(
                    "the funding service did not fund {} account(s) within {}s",
                    account_ids.len(),
                    FUNDING_DEADLINE.as_secs()
                )
            })?
    }
}

/// Returns the faucet the chain charges fees in, as the genesis header's protocol configuration
/// names it.
pub async fn fee_faucet_id<AUTH: TransactionAuthenticator + Sync + 'static>(
    client: &Client<AUTH>,
) -> Result<AccountId> {
    let (genesis, _) = client
        .get_block_header_by_num(BlockNumber::GENESIS)
        .await?
        .context("genesis block header is not in the client's store")?;

    Ok(client
        .get_protocol_config(genesis.protocol_config_commitment())
        .await?
        .fee_asset_id()
        .faucet_id())
}

/// Builds the [`FeeFunder`] a run draws the native fee asset from, talking to the funding service
/// at `funding_service`.
///
/// Yields no funder when `funding_service` names none, which is all a fee-free chain needs.
pub fn load(funding_service: Option<&str>) -> Result<Option<Arc<dyn FeeFunder>>> {
    let Some(url) = funding_service.map(str::trim).filter(|url| !url.is_empty()) else {
        return Ok(None);
    };

    validate_url(url)?;

    Ok(Some(Arc::new(FundingServiceFunder::new(url, FUNDING_AMOUNT)?)))
}

/// Returns the funding service URL named by [`FUNDING_SERVICE_ENV`], for runners that read it
/// themselves rather than taking it as an argument.
pub fn funding_service_from_env() -> Option<String> {
    std::env::var(FUNDING_SERVICE_ENV).ok().filter(|url| !url.trim().is_empty())
}

/// Checks that `url` is one the HTTP client can use, so a typo is reported where it was given
/// rather than on the first funding request.
fn validate_url(url: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(url)
        .with_context(|| format!("the funding service URL {url} is not a URL"))?;

    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("the funding service URL {url} must be http or https, not {}", parsed.scheme());
    }

    Ok(())
}

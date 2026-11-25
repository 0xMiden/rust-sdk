//! Returns the native asset that funded test accounts did not spend to the funding service.
//!
//! When [`REFUND_ENV`] is set, each account that the funding service funds is recorded with its
//! client. After the test, [`run_test`] opens each of these clients again, and each funded account
//! sends its balance to the funding service in a public pay-to-ID note.
//!
//! A refund never fails the test. A failure is logged, and the account keeps its balance.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures::FutureExt;
use miden_client::account::AccountId;
use miden_client::asset::FungibleAsset;
use miden_client::note::{Note, NoteType, P2idNote};
use miden_client::testing::common::TestClient;
use miden_client::testing::fee::FeeFunder;
use miden_client::transaction::TransactionRequestBuilder;
use miden_protocol::MAX_TX_EXECUTION_CYCLES;
use miden_protocol::transaction::TransactionFee;
use miden_standards::tx_script::SendWalletNotesTransactionScript;
use rand::RngExt;
use serde::Deserialize;
use tracing::{info, warn};

use crate::ClientConfig;
use crate::funding::funding_service_from_env;

// CONSTANTS
// ================================================================================================

/// Env var that turns refunds on. `1` turns it on, otherwise it's off by default.
pub const REFUND_ENV: &str = "MIDEN_TEST_RETURN_UNSPENT_FUNDING";

/// The maximum time for the refund of all accounts of a test.
///
/// The test runner stops a test after 6 minutes. This deadline keeps the refund inside that limit
/// for a test that ends before it.
const REFUND_DEADLINE: Duration = Duration::from_secs(120);

// REFUNDS
// ================================================================================================

/// The files of a client that a test created: its store and its keystore.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct ClientFiles {
    pub(crate) store_path: PathBuf,
    pub(crate) keystore_path: PathBuf,
}

/// Records the accounts that the clients of a test fund, and refunds these accounts after the test.
#[derive(Debug)]
pub struct Refunds {
    /// Base URL of the funding service. Its account receives the refunds.
    funding_service: String,
    /// The funded accounts of each client.
    funded: Mutex<BTreeMap<ClientFiles, BTreeSet<AccountId>>>,
}

impl Refunds {
    /// Builds the refunds that the environment asks for, or `None` when [`REFUND_ENV`] does not
    /// turn them on.
    pub fn from_env() -> Result<Option<Self>> {
        let enabled = std::env::var(REFUND_ENV).is_ok_and(|value| value == "1");
        if !enabled {
            return Ok(None);
        }

        let Some(funding_service) = funding_service_from_env() else {
            bail!(
                "{REFUND_ENV} asks for refunds, but no funding service is set to receive them. Set \
                 {}",
                crate::funding::FUNDING_SERVICE_ENV
            );
        };

        Ok(Some(Self {
            funding_service: funding_service.trim().trim_end_matches('/').to_string(),
            funded: Mutex::new(BTreeMap::new()),
        }))
    }

    /// Returns a funder that funds through `funder` and records each account that it funds as an
    /// account of the client in `files`.
    pub(crate) fn recording_funder(
        self: &Arc<Self>,
        funder: Arc<dyn FeeFunder>,
        files: ClientFiles,
    ) -> Arc<dyn FeeFunder> {
        Arc::new(RecordingFunder { funder, refunds: Arc::clone(self), files })
    }

    /// Sends the balances of the funded accounts back to the funding service.
    ///
    /// `config` builds the clients again. It must point at the same node as the test.
    pub(crate) async fn refund(&self, config: &ClientConfig) {
        let funded = std::mem::take(
            &mut *self.funded.lock().expect("the funded map lock is never poisoned"),
        );
        if funded.is_empty() {
            return;
        }

        let target = match self.target().await {
            Ok(target) => target,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "Cannot refund the test accounts");
                return;
            },
        };

        for (files, account_ids) in funded {
            if let Err(err) = refund_client(config, &files, target, &account_ids).await {
                warn!(
                    store = %files.store_path.display(),
                    error = %format!("{err:#}"),
                    "Cannot refund the accounts of a test client",
                );
            }
        }
    }

    /// Returns the account of the funding service, which receives the refunds.
    async fn target(&self) -> Result<AccountId> {
        #[derive(Deserialize)]
        struct Status {
            account_id: String,
        }

        let url = format!("{}/status", self.funding_service);
        let status: Status = reqwest::get(&url)
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("failed to read the funding service status at {url}"))?
            .json()
            .await
            .with_context(|| format!("failed to parse the funding service status at {url}"))?;

        AccountId::from_hex(&status.account_id)
            .map_err(|err| anyhow!("the funding service named an invalid account: {err}"))
    }
}

/// Opens the client in `files` and refunds each account in `account_ids` to `target`.
async fn refund_client(
    config: &ClientConfig,
    files: &ClientFiles,
    target: AccountId,
    account_ids: &BTreeSet<AccountId>,
) -> Result<()> {
    let mut client = config.open_client(files).await?;
    client.sync_state().await.context("failed to sync the client")?;

    let fee_faucet_id = client.fee_faucet_id().await?;
    let fee_parameters = client.get_latest_block_header().await?.fee_parameters().clone();
    let max_fee = TransactionFee::new(MAX_TX_EXECUTION_CYCLES)?
        .compute_fee(&fee_parameters)?
        .as_u64();

    // A failure for one account does not stop the refund of the other accounts.
    for &account_id in account_ids {
        if let Err(err) =
            refund_account(&mut client, account_id, target, fee_faucet_id, max_fee).await
        {
            warn!(
                %account_id,
                error = %format!("{err:#}"),
                "Cannot refund a test account",
            );
        }
    }

    Ok(())
}

/// Sends the native asset balance of `account_id` to `target` in a public pay-to-ID note.
///
/// The account keeps `max_fee` to pay for the refund transaction. It sends nothing when its balance
/// does not exceed `max_fee`. It does not wait for the transaction to commit, because nothing after
/// the test depends on it.
async fn refund_account(
    client: &mut TestClient,
    account_id: AccountId,
    target: AccountId,
    fee_faucet_id: AccountId,
    max_fee: u64,
) -> Result<()> {
    let balance = client.account_reader(account_id).get_balance(fee_faucet_id).await?.as_u64();
    let amount = balance.saturating_sub(max_fee);
    if amount == 0 {
        return Ok(());
    }

    let account = client
        .get_account(account_id)
        .await?
        .with_context(|| format!("account {account_id} is not in the store"))?;

    let note: Note = P2idNote::builder()
        .sender(account_id)
        .target(target)
        .asset(FungibleAsset::new(fee_faucet_id, amount)?)
        .note_type(NoteType::Public)
        .serial_number(client.rng().random())
        .build()?
        .into();

    // A custom script is necessary because the standard request uses the mint script for a faucet
    // account. The tests fund faucets too, and a faucet must send the native asset that it holds.
    let script =
        SendWalletNotesTransactionScript::new(&account.code_interface(), &[note.clone().into()])
            .context("the account cannot send assets in a note")?;
    let request = TransactionRequestBuilder::new()
        .custom_script(script.tx_script().clone())
        .script_arg(script.tx_script_args())
        .expected_output_recipients(vec![note.recipient().clone()])
        .build()?;

    let tx_id = Box::pin(client.submit_new_transaction(account_id, request)).await?;
    info!(%account_id, %tx_id, amount, "Refunded a test account");
    Ok(())
}

// RECORDING FUNDER
// ================================================================================================

/// A fee funder that records each account that it funds in [`Refunds`], as an account of the client
/// in `files`.
#[derive(Debug)]
struct RecordingFunder {
    funder: Arc<dyn FeeFunder>,
    refunds: Arc<Refunds>,
    files: ClientFiles,
}

#[async_trait::async_trait(?Send)]
impl FeeFunder for RecordingFunder {
    async fn fund(&self, account_ids: &[AccountId]) -> Result<Vec<(AccountId, Note)>> {
        let funded = self.funder.fund(account_ids).await?;
        self.refunds
            .funded
            .lock()
            .expect("the funded map lock is never poisoned")
            .entry(self.files.clone())
            .or_default()
            .extend(funded.iter().map(|(account_id, _)| *account_id));
        Ok(funded)
    }
}

// TEST RUNNER
// ================================================================================================

/// Runs `test` with `client_config`, then refunds the accounts of the clients the test created.
///
/// The refund also runs when the test returns an error or panics. A panic continues after the
/// refund.
pub async fn run_test<F, Fut>(client_config: ClientConfig, test: F) -> Result<()>
where
    F: FnOnce(ClientConfig) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let result = AssertUnwindSafe(test(client_config.clone())).catch_unwind().await;

    // A refund never fails the test. A panic in the refund is logged, and the deadline keeps the
    // refund inside the time limit of the test runner.
    if let Some(refunds) = &client_config.refunds {
        let refund = AssertUnwindSafe(refunds.refund(&client_config)).catch_unwind();
        match tokio::time::timeout(REFUND_DEADLINE, refund).await {
            Ok(Ok(())) => {},
            Ok(Err(_)) => warn!("The refund of the test accounts panicked"),
            Err(_) => warn!("The refund of the test accounts did not finish before its deadline"),
        }
    }

    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

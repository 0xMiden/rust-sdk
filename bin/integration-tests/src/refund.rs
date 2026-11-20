//! Returns the native fee asset that test accounts did not spend to the account that funded them.
//!
//! A test account receives more of the native asset than its transactions cost. On a chain that
//! outlives the run, such as testnet, the remainder is lost unless the account sends it back. When
//! [`REFUND_ENV`] is set, every client that a [`ClientConfig`] creates is recorded. After the test
//! ends, [`run_test`] opens each recorded client again and sends the balance of each of its
//! accounts back to the funding service in a public pay-to-ID note. The funding service consumes
//! such a note as a deposit.
//!
//! A refund never fails the test. An account that cannot send the refund keeps its balance, and the
//! failure is logged.

use std::collections::BTreeSet;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use futures::FutureExt;
use miden_client::account::AccountId;
use miden_client::asset::FungibleAsset;
use miden_client::keystore::Keystore;
use miden_client::note::{Note, NoteType, P2idNote};
use miden_client::store::AccountStatus;
use miden_client::testing::common::TestClient;
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

/// Env var that turns refunds on. Any value other than `0`, `false` or the empty string turns them
/// on.
///
/// Refunds are off by default. A chain that the run throws away, such as the testing node, gains
/// nothing from them, and each refund costs a proof.
pub const REFUND_ENV: &str = "MIDEN_TEST_REFUND_FUNDS";

/// A balance must exceed this number of the largest transaction fees to be refunded. A smaller
/// balance returns less than the refund costs the chain.
const MIN_REFUND_IN_MAX_FEES: u64 = 2;

// REFUNDS
// ================================================================================================

/// The files of a client that a test created: its store and its keystore.
#[derive(Debug, Clone)]
pub(crate) struct ClientFiles {
    pub(crate) store_path: PathBuf,
    pub(crate) keystore_path: PathBuf,
}

/// Records the clients of a test and refunds their accounts after the test.
#[derive(Debug)]
pub struct Refunds {
    /// Base URL of the funding service. Its account receives the refunds.
    funding_service: String,
    clients: Mutex<Vec<ClientFiles>>,
}

impl Refunds {
    /// Builds the refunds that the environment asks for, or `None` when [`REFUND_ENV`] does not
    /// turn them on.
    pub fn from_env() -> Result<Option<Self>> {
        let enabled = std::env::var(REFUND_ENV)
            .is_ok_and(|value| !matches!(value.trim(), "" | "0" | "false"));
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
            clients: Mutex::new(Vec::new()),
        }))
    }

    /// Records a client, so that [`Self::refund`] returns the balances of its accounts.
    pub(crate) fn record(&self, files: ClientFiles) {
        self.clients.lock().expect("the refund list lock is never poisoned").push(files);
    }

    /// Sends the balances of the accounts of every recorded client back to the funding service.
    ///
    /// `config` builds the clients again. It must point at the same node as the test.
    pub(crate) async fn refund(&self, config: &ClientConfig) {
        let clients = std::mem::take(
            &mut *self.clients.lock().expect("the refund list lock is never poisoned"),
        );
        if clients.is_empty() {
            return;
        }

        let target = match self.target().await {
            Ok(target) => target,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "Cannot refund the test accounts");
                return;
            },
        };

        // Two clients of a test can hold the key of the same public account. Each account is
        // refunded once, because a second refund conflicts with the first one in the mempool.
        let mut refunded = BTreeSet::new();
        for files in clients {
            if let Err(err) = refund_client(config, &files, target, &mut refunded).await {
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

/// Opens the client in `files` and refunds each of its accounts to `target`.
///
/// It skips the accounts in `refunded`, and adds each account that it refunds.
async fn refund_client(
    config: &ClientConfig,
    files: &ClientFiles,
    target: AccountId,
    refunded: &mut BTreeSet<AccountId>,
) -> Result<()> {
    let mut client = config.open_client(&files.store_path, &files.keystore_path).await?;
    client.sync_state().await.context("failed to sync the client")?;

    let fee_faucet_id = client.fee_faucet_id().await?;
    let fee_parameters = client.get_latest_block_header().await?.fee_parameters().clone();
    let max_fee = TransactionFee::new(MAX_TX_EXECUTION_CYCLES)?
        .compute_fee(&fee_parameters)?
        .as_u64();

    for (header, status) in client.get_account_headers().await? {
        let account_id = header.id();
        if account_id == target
            || refunded.contains(&account_id)
            || !can_refund(&client, account_id, &status).await?
        {
            continue;
        }

        let balance = client.account_reader(account_id).get_balance(fee_faucet_id).await?.as_u64();
        if balance <= max_fee.saturating_mul(MIN_REFUND_IN_MAX_FEES) {
            continue;
        }

        // The refund transaction pays its own fee from what stays in the account.
        let amount = balance - max_fee;
        refunded.insert(account_id);
        if let Err(err) =
            refund_account(&mut client, account_id, target, fee_faucet_id, amount).await
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

/// Returns whether the client can send a transaction from `account_id`.
///
/// An account that never transacted holds only its unspent funding note, an account whose state is
/// locked cannot transact, and an account without a key in the keystore is only watched.
async fn can_refund(
    client: &TestClient,
    account_id: AccountId,
    status: &AccountStatus,
) -> Result<bool> {
    if !matches!(status, AccountStatus::Tracked) {
        return Ok(false);
    }

    let keys = client
        .keystore()
        .get_account_key_commitments(&account_id)
        .await
        .with_context(|| format!("failed to read the keys of account {account_id}"))?;
    Ok(!keys.is_empty())
}

/// Sends `amount` of the native asset from `account_id` to `target` in a public pay-to-ID note.
///
/// It does not wait for the transaction to commit, because nothing after the test depends on it.
async fn refund_account(
    client: &mut TestClient,
    account_id: AccountId,
    target: AccountId,
    fee_faucet_id: AccountId,
    amount: u64,
) -> Result<()> {
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

    // The standard request picks the mint script of a faucet whenever each note holds one fungible
    // asset, also for an asset that the faucet did not issue. The faucets of the tests also hold
    // the native asset, so the request always uses the wallet script, which sends assets that the
    // account holds.
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

    if let Some(refunds) = client_config.refunds() {
        refunds.refund(&client_config).await;
    }

    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

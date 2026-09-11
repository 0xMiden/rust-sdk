//! Supplies the native fee asset to the accounts the test helpers create, so the suite can run
//! against a chain that charges transaction fees.
//!
//! Both runners give every test its own process, so a process claims a wallet before its first
//! payment, taking the first advisory lock in the pool that is free, and holds it until it exits.
//! Claiming rather than assigning by ordinal is what keeps a small pool useful under concurrency.

use std::fmt;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use miden_client::account::{AccountFile, AccountId};
use miden_client::asset::FungibleAsset;
use miden_client::block::BlockNumber;
use miden_client::keystore::Keystore;
use miden_client::note::{Note, NoteType, P2idNote};
use miden_client::testing::common::TestClient;
use miden_client::testing::fee::FeeFunder;
use miden_client::transaction::{TransactionId, TransactionRequest, TransactionRequestBuilder};
use miden_client::{ClientError, Deserializable};
use rand::RngExt;
use rustix::fs::{FlockOperation, flock};
use rustix::io::Errno;
use tokio::sync::Mutex;
use tracing::warn;

use crate::config::ClientConfig;

// CONSTANTS
// ================================================================================================

/// Env var naming the funder account file or directory, mirroring the `--funders` argument.
pub const FUNDER_ACCOUNTS_ENV: &str = "MIDEN_FUNDER_ACCOUNTS_DIR";

/// Amount of the native fee asset, in base units, each funded account receives. A fee runs a few
/// tens of thousands of base units, so this covers far more than any one test spends.
const FUNDING_AMOUNT: u64 = 10_000_000;

/// How long to wait before a rejected payment is submitted again.
const STALE_WALLET_RETRY_DELAY: Duration = Duration::from_secs(5);

// LOADING
// ================================================================================================

/// Returns the funder path named by [`FUNDER_ACCOUNTS_ENV`], for runners that read it themselves
/// rather than taking it as an argument. [`load`] decides what a path holding no funder means.
pub fn funders_path_from_env() -> Option<PathBuf> {
    std::env::var_os(FUNDER_ACCOUNTS_ENV).map(PathBuf::from)
}

/// Loads the wallets at `funders` as a [`FeeFunder`] paying out of whichever one is free. The
/// funder client is built from `client_config`'s endpoints.
///
/// Yields no funder when `funders` names no funder file. A file that is there but cannot be used as
/// a funder is still an error.
pub fn load(
    client_config: &ClientConfig,
    funders: Option<&Path>,
) -> Result<Option<Arc<dyn FeeFunder>>> {
    let wallets = load_funders(funders)?;
    if wallets.is_empty() {
        return Ok(None);
    }

    Ok(Some(Arc::new(Funder::new(client_config, wallets))))
}

/// Loads the funder wallets at `path`, which is either one `.mac` file or a directory of them, and
/// none at all when `path` names no such file.
fn load_funders(path: Option<&Path>) -> Result<Vec<AccountFile>> {
    let Some(path) = path.filter(|path| !path.as_os_str().is_empty()) else {
        return Ok(Vec::new());
    };

    let paths = if path.is_dir() {
        let mut mac_files: Vec<PathBuf> = std::fs::read_dir(path)
            .with_context(|| format!("failed to read funder directory {}", path.display()))?
            .map(|entry| Ok(entry?.path()))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|path| path.extension().is_some_and(|ext| ext == "mac"))
            .collect();

        // Every process has to agree on the order, or the scan offsets would not spread concurrent
        // tests over distinct wallets.
        mac_files.sort();
        mac_files
    } else if path.is_file() {
        vec![path.to_path_buf()]
    } else {
        Vec::new()
    };

    // A private funder's state lives only in the file, so sharing one across processes would build
    // every transaction from the same stale snapshot. A public one is re-read from the chain.
    paths
        .iter()
        .map(|path| {
            let bytes = std::fs::read(path)
                .with_context(|| format!("failed to read funder {}", path.display()))?;
            let funder = AccountFile::read_from_bytes(&bytes).map_err(|err| {
                anyhow::anyhow!("failed to deserialize {}: {err}", path.display())
            })?;

            let id = funder.account.id();
            if !id.is_public() {
                bail!("funder {id} in {} must be public to be shared", path.display());
            }
            if funder.auth_secret_keys.is_empty() {
                bail!("funder {id} in {} carries no secret key to sign with", path.display());
            }

            Ok(funder)
        })
        .collect()
}

// FUNDER
// ================================================================================================

/// The pool of pre-funded wallets a run was given, paying out of whichever one is free.
struct Funder {
    /// What the funder client is built from: the test's endpoints, and no funder of its own.
    client_config: ClientConfig,
    wallets: Vec<AccountFile>,
    /// Where this test starts scanning, so concurrent tests do not all try the same wallet first.
    scan_from: usize,
    /// Built on the first funding request.
    state: Mutex<Option<FunderState>>,
}

/// The wallet this process pays from.
struct FunderState {
    /// Holds every wallet's key. Separate from the clients the test builds, so consecutive payments
    /// from one wallet chain off each other's nonce.
    client: TestClient,
    /// Claim on the wallet, released when the process exits. Whichever process takes the wallet
    /// next reads its state from the chain, so [`Funder::flush`] runs before the release.
    lock: AccountLock,
    /// The last payment the node accepted, until a block carries it.
    in_flight: Option<TransactionId>,
}

impl Funder {
    fn new(client_config: &ClientConfig, wallets: Vec<AccountFile>) -> Self {
        Self {
            client_config: client_config
                .clone()
                .with_fee_funder(None)
                .with_note_transport_endpoint(None),
            wallets,
            scan_from: rand::rng().random::<u32>() as usize,
            state: Mutex::new(None),
        }
    }

    /// Claims a wallet to pay from and builds the client that pays with it.
    async fn claim_state(&self) -> Result<FunderState> {
        let lock = self.claim()?;
        let client = self.build_client().await?;

        Ok(FunderState { client, lock, in_flight: None })
    }

    /// Claims a wallet to pay from, waiting only if every wallet in the pool is busy.
    fn claim(&self) -> Result<AccountLock> {
        for offset in 0..self.wallets.len() {
            let wallet = &self.wallets[(self.scan_from + offset) % self.wallets.len()];
            if let Some(lock) = AccountLock::try_acquire(wallet.account.id())? {
                return Ok(lock);
            }
        }

        // Everything is busy. Waiting on this test's own starting wallet spreads the waiters. A
        // wallet is held for as long as the process that claimed it runs, so this waits for one of
        // them to exit. Raise `MIDEN_NUM_FUNDER_WALLETS` if a run reaches here often.
        let wallet = &self.wallets[self.scan_from % self.wallets.len()];
        warn!(
            wallets = self.wallets.len(),
            funder_id = %wallet.account.id(),
            "Every funder wallet is claimed, waiting for one to be released",
        );

        AccountLock::acquire(wallet.account.id())
    }

    async fn build_client(&self) -> Result<TestClient> {
        let mut client = self
            .client_config
            .clone()
            .into_unsynced_client()
            .await
            .context("failed to build the funder client")?;

        // Some tests create their accounts before waiting for the node, so the wait happens here.
        client.wait_for_node().await;
        client.sync_state().await.context("failed to sync the funder client")?;

        for wallet in &self.wallets {
            let id = wallet.account.id();
            for key in &wallet.auth_secret_keys {
                client.keystore().add_key(key, id).await.context("failed to add a funder key")?;
            }
        }

        Ok(client)
    }

    /// Pays every account in `targets` from the claimed wallet in a single transaction, returning
    /// each target paired with the note carrying its funds.
    ///
    /// One transaction rather than one per target: each costs a fee and a proof.
    async fn pay(
        &self,
        state: &mut FunderState,
        targets: &[AccountId],
    ) -> Result<Vec<(AccountId, Note)>> {
        let wallet_id = state.lock.account_id();
        let client = &mut state.client;

        // Imported once per client. A re-import of a wallet this client has already paid from
        // fails, because its local nonce is ahead of the chain's until that payment commits.
        if client.account_reader(wallet_id).nonce().await.is_err() {
            client
                .import_account_by_id(wallet_id)
                .await
                .with_context(|| format!("failed to import funder {wallet_id}"))?;
        }

        let (genesis, _) = client
            .get_block_header_by_num(BlockNumber::GENESIS)
            .await?
            .context("genesis block header is not in the funder client's store")?;
        let fee_faucet_id = genesis.fee_parameters().fee_faucet_id();

        // The callback flag is part of the vault key, and both the wallet's balance and `pay_fee`
        // use plain assets, so the note has to carry the same flag to be spendable.
        let asset = FungibleAsset::new(fee_faucet_id, FUNDING_AMOUNT)
            .context("failed to build the native fee asset")?;

        // Built here rather than through `build_pay_to_id`, which describes a single payment. Notes
        // stay paired with their target, so nothing matches them back by position.
        let funded = targets
            .iter()
            .map(|target| {
                let note: Note = P2idNote::builder()
                    .sender(wallet_id)
                    .target(*target)
                    .asset(asset)
                    .note_type(NoteType::Private)
                    .generate_serial_number(client.rng())
                    .build()
                    .context("failed to build a funding note")?
                    .into();

                Ok((*target, note))
            })
            .collect::<Result<Vec<(AccountId, Note)>>>()?;

        let request = TransactionRequestBuilder::new()
            .own_output_notes(funded.iter().map(|(_, note)| note.clone()))
            .build()
            .context("failed to build the funding transaction request")?;

        let tx_id = Self::submit_payment(client, wallet_id, request).await.with_context(|| {
            format!("funder {wallet_id} failed to pay {} accounts", targets.len())
        })?;
        state.in_flight = Some(tx_id);

        Ok(funded)
    }

    /// Submits `request` from `wallet_id`, trying a second time after a sync if the node rejects
    /// the first attempt.
    async fn submit_payment(
        client: &mut TestClient,
        wallet_id: AccountId,
        request: TransactionRequest,
    ) -> Result<TransactionId> {
        let rejection =
            match Box::pin(client.submit_new_transaction(wallet_id, request.clone())).await {
                Ok(tx_id) => return Ok(tx_id),
                Err(err) => err,
            };
        if !is_safe_to_retry(&rejection) {
            return Err(anyhow::Error::new(rejection));
        }

        warn!(
            funder_id = %wallet_id,
            error = %rejection,
            "The funder payment was rejected, retrying after a sync",
        );
        tokio::time::sleep(STALE_WALLET_RETRY_DELAY).await;
        client
            .sync_state()
            .await
            .context("failed to sync the funder client before a retry")?;

        Box::pin(client.submit_new_transaction(wallet_id, request))
            .await
            .map_err(|err| {
                anyhow::Error::new(err).context(format!(
                    "the retry was rejected too, after the first attempt failed with: {rejection}"
                ))
            })
    }
}

/// Returns whether a failed submission definitely left the node's state untouched, so the same
/// payment can be built and sent again.
///
/// A transaction the node accepted, or may have accepted, has already moved the wallet's nonce. A
/// second copy of it would be rejected, and would hide the first.
fn is_safe_to_retry(err: &ClientError) -> bool {
    !matches!(
        err,
        ClientError::ApplyTransactionAfterSubmitFailed { .. }
            | ClientError::SubmissionOutcomeUnknown { .. }
    )
}

#[async_trait::async_trait(?Send)]
impl FeeFunder for Funder {
    async fn fund(&self, account_ids: &[AccountId]) -> Result<Vec<(AccountId, Note)>> {
        if account_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut guard = self.state.lock().await;
        let state = match guard.as_mut() {
            Some(state) => state,
            None => guard.insert(self.claim_state().await?),
        };

        self.pay(state, account_ids).await
    }

    async fn flush(&self) -> Result<()> {
        let mut guard = self.state.lock().await;
        let Some(state) = guard.as_mut() else {
            return Ok(());
        };
        let Some(tx_id) = state.in_flight.take() else {
            return Ok(());
        };
        let wallet_id = state.lock.account_id();

        state
            .client
            .wait_for_tx(tx_id)
            .await
            .with_context(|| format!("the payment from funder {wallet_id} never committed"))
    }
}

impl fmt::Debug for Funder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Funder")
            .field("rpc_endpoint", &self.client_config.rpc_endpoint)
            .field("wallets", &self.wallets.iter().map(|w| w.account.id()).collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

// ACCOUNT LOCK
// ================================================================================================

/// An advisory lock over one account, shared across the test processes on this machine.
///
/// Held by the funder above over the wallet it claimed, and by the agglayer tests over the accounts
/// they share. The lock file lives in the temp directory, so a read-only account file is never
/// written to, and releases on drop or when the holding process dies.
pub struct AccountLock {
    file: File,
    account_id: AccountId,
}

impl AccountLock {
    /// Takes the lock, waiting for it if another process holds it.
    pub fn acquire(account_id: AccountId) -> Result<Self> {
        let file = Self::open(account_id)?;
        flock(&file, FlockOperation::LockExclusive)
            .with_context(|| format!("failed to lock account {account_id}"))?;

        Ok(Self { file, account_id })
    }

    /// Takes the lock if it is free, returning `None` if another process holds it.
    pub fn try_acquire(account_id: AccountId) -> Result<Option<Self>> {
        let file = Self::open(account_id)?;
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => Ok(Some(Self { file, account_id })),
            Err(Errno::WOULDBLOCK) => Ok(None),
            Err(err) => {
                Err(anyhow::Error::new(err).context(format!("failed to lock account {account_id}")))
            },
        }
    }

    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    fn open(account_id: AccountId) -> Result<File> {
        let path = std::env::temp_dir().join(format!("miden-account-{}.lock", account_id.to_hex()));
        File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("failed to open account lock file {}", path.display()))
    }
}

impl Drop for AccountLock {
    fn drop(&mut self) {
        // Closing the file releases the lock, so a failure here only means it is released a moment
        // later than intended.
        let _ = flock(&self.file, FlockOperation::Unlock);
    }
}

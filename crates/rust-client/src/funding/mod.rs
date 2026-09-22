//! Requests native fee assets from a development faucet.
//!
//! Enable the opt-in `funding` feature to use these helpers. Native applications need a Tokio
//! runtime. Browser applications use the same faucet protocol through Fetch. The faucet issues
//! public P2ID notes. The receiving account must expose `BasicWallet`'s `receive_asset` procedure
//! and use an authenticator that can sign the consumption transaction. Multisig accounts that
//! require a separate signing flow should use `request_funding_note`.

use alloc::boxed::Box;
use alloc::string::{String, ToString};
use core::future::Future;
use core::pin::pin;
use core::time::Duration;

use futures::future::{Either, select};
use miden_protocol::crypto::hash::sha2::Sha256;
use serde::Deserialize;

use crate::account::AccountId;
use crate::address::{Address, AddressId, NetworkId};
use crate::asset::AssetAmount;
use crate::auth::TransactionAuthenticator;
use crate::note::{Note, NoteId};
use crate::store::TransactionFilter;
use crate::transaction::{TransactionId, TransactionRequestBuilder, TransactionStatus};
use crate::{Client, ClientError};

/// Options for development funding. Amounts are in the native fee asset's base units.
#[derive(Clone, Debug)]
pub struct FundingOptions {
    /// Faucet API base URL. Testnet and devnet have defaults; other networks require this field.
    pub faucet_url: Option<String>,
    /// Amount to request. `None` uses the faucet's advertised base amount.
    pub amount: Option<u64>,
    /// Limit for each stage: faucet request, note discovery, and transaction confirmation.
    /// Execution and proving use the client's existing configuration. Must be between 1 ms and
    /// 2,147,483,647 ms, the browser timer limit.
    pub timeout: Duration,
    /// Delay between chain syncs. Uses the same bounds as `timeout`.
    pub poll_interval: Duration,
}

impl Default for FundingOptions {
    fn default() -> Self {
        Self {
            faucet_url: None,
            amount: None,
            timeout: Duration::from_secs(60),
            poll_interval: Duration::from_secs(5),
        }
    }
}

/// Receipt returned when the faucet accepts a funding request. The note can still be pending.
#[derive(Clone, Debug)]
pub struct FundingNote {
    pub note_id: NoteId,
    /// Faucet transaction that creates the note.
    pub transaction_id: TransactionId,
    pub faucet_id: AccountId,
    pub amount: u64,
}

/// Confirmed funding result. The balance is read after the consumption fee is paid.
#[derive(Clone, Debug)]
pub struct FundingResult {
    pub note_id: NoteId,
    /// Transaction that consumes the funding note.
    pub transaction_id: TransactionId,
    pub balance: AssetAmount,
}

/// A funding failure. After minting, errors retain the IDs needed to resume manually.
#[derive(Debug, thiserror::Error)]
pub enum FundingError {
    #[error("invalid funding options or faucet response: {0}")]
    Invalid(String),
    #[error("faucet request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("funding client operation failed: {0}")]
    Client(#[from] ClientError),
    #[error("funding timed out while waiting for {0}")]
    Timeout(&'static str),
    #[error("funding transaction {0} was discarded: {1}")]
    Discarded(TransactionId, String),
    #[error("funding note {note_id}, consumption transaction {transaction_id:?}: {source}")]
    AfterRequest {
        note_id: NoteId,
        transaction_id: Option<TransactionId>,
        #[source]
        source: Box<FundingError>,
    },
}

impl FundingOptions {
    fn validate(&self) -> Result<(), FundingError> {
        let min = Duration::from_millis(1);
        let max = Duration::from_millis(i32::MAX as u64);
        if !(min..=max).contains(&self.timeout) || !(min..=max).contains(&self.poll_interval) {
            return Err(FundingError::Invalid(
                "timeout and poll interval must be between 1 and 2147483647 milliseconds".into(),
            ));
        }
        if let Some(amount) = self.amount {
            if amount == 0 {
                return Err(FundingError::Invalid("amount must be positive".into()));
            }
            AssetAmount::new(amount).map_err(|error| FundingError::Invalid(error.to_string()))?;
        }
        Ok(())
    }
}

impl<AUTH: TransactionAuthenticator + Sync + 'static> Client<AUTH> {
    /// Requests a public P2ID note containing the chain's native fee asset.
    ///
    /// This does not consume the note or sign a transaction. The client checks the faucet's asset
    /// against the registered protocol configuration before requesting funds. The request is never
    /// retried automatically: a timeout during minting can leave a note on the chain.
    pub async fn request_funding_note(
        &mut self,
        account_id: AccountId,
        options: &FundingOptions,
    ) -> Result<FundingNote, FundingError> {
        options.validate()?;
        within(options.timeout, "the faucet (minting may already have succeeded)", async {
            self.sync_chain().await?;
            let header = self.get_latest_block_header().await?;
            let config = self.get_protocol_config(header.protocol_config_commitment()).await?;
            let network = self.network_id().await?;
            let endpoint = match options.faucet_url.as_deref() {
                Some(endpoint) => endpoint,
                None => match network {
                    NetworkId::Testnet => "https://faucet-api.testnet.miden.io",
                    NetworkId::Devnet => "https://faucet-api.devnet.miden.io",
                    _ => {
                        return Err(FundingError::Invalid(
                            "this network requires a faucet_url".into(),
                        ));
                    },
                },
            };
            request_note(
                endpoint,
                account_id,
                config.fee_asset_id().faucet_id(),
                network,
                options.amount,
            )
            .await
        })
        .await
    }

    /// Funds a tracked account and waits for its consumption transaction to commit.
    ///
    /// A fresh wallet can pay its first fee from this note. The amount must cover that fee. Only
    /// the returned funding note is consumed. Existing balances do not suppress funding. After a
    /// failure with a note ID, resume that note instead of requesting funds again.
    pub async fn fund_account(
        &mut self,
        account_id: AccountId,
        options: &FundingOptions,
    ) -> Result<FundingResult, FundingError> {
        options.validate()?;
        // Reject untracked and watched accounts before the faucet creates a note.
        self.get_native_account_record(account_id).await?;
        let receipt = self.request_funding_note(account_id, options).await?;
        let mut transaction_id = None;
        let result = async {
            let note: Note = within(options.timeout, "the funding note", async {
                loop {
                    self.sync_chain().await?;
                    if let Some(record) = self.get_input_note(receipt.note_id).await? {
                        if record.is_consumed() || record.is_processing() {
                            return Err(FundingError::Invalid(
                                "funding note is already consumed or being processed".into(),
                            ));
                        }
                        if record.is_committed() {
                            return record
                                .try_into()
                                .map_err(ClientError::from)
                                .map_err(FundingError::from);
                        }
                    }
                    sleep(options.poll_interval).await;
                }
            })
            .await?;
            let request = TransactionRequestBuilder::new()
                .build_consume_notes(vec![note])
                .map_err(ClientError::from)?;
            let id = self.submit_new_transaction(account_id, request).await.map_err(|error| {
                transaction_id = match &error {
                    ClientError::SubmissionOutcomeUnknown { transaction, .. } => {
                        Some(transaction.id())
                    },
                    ClientError::ApplyTransactionAfterSubmitFailed { pending_update, .. } => {
                        Some(pending_update.executed_transaction().id())
                    },
                    _ => None,
                };
                FundingError::Client(error)
            })?;
            transaction_id = Some(id);
            within(options.timeout, "the consumption transaction", async {
                loop {
                    self.sync_chain().await?;
                    let transactions =
                        self.get_transactions(TransactionFilter::Ids(vec![id])).await?;
                    if let Some(record) = transactions.first() {
                        match &record.status {
                            TransactionStatus::Committed { .. } => return Ok(()),
                            TransactionStatus::Discarded(cause) => {
                                return Err(FundingError::Discarded(id, cause.to_string()));
                            },
                            TransactionStatus::Pending => {},
                        }
                    }
                    sleep(options.poll_interval).await;
                }
            })
            .await?;
            let balance = self.account_reader(account_id).get_balance(receipt.faucet_id).await?;
            Ok(FundingResult {
                note_id: receipt.note_id,
                transaction_id: id,
                balance,
            })
        }
        .await;
        result.map_err(|source| FundingError::AfterRequest {
            note_id: receipt.note_id,
            transaction_id,
            source: Box::new(source),
        })
    }
}

// Use the same runtimes as the client's RPC transport.
#[cfg(not(target_arch = "wasm32"))]
async fn sleep(duration: Duration) {
    tokio::time::sleep(duration).await;
}

#[cfg(target_arch = "wasm32")]
async fn sleep(duration: Duration) {
    gloo_timers::future::sleep(duration).await;
}

async fn within<T>(
    timeout: Duration,
    stage: &'static str,
    future: impl Future<Output = Result<T, FundingError>>,
) -> Result<T, FundingError> {
    // Give the deadline priority when both futures are ready.
    match select(pin!(sleep(timeout)), pin!(future)).await {
        Either::Left(_) => Err(FundingError::Timeout(stage)),
        Either::Right((result, _)) => result,
    }
}

#[derive(Deserialize)]
struct Metadata {
    id: String,
    base_amount: u64,
}
#[derive(Deserialize)]
struct Challenge {
    challenge: String,
    target: u64,
}
#[derive(Deserialize)]
struct Mint {
    note_id: String,
    tx_id: String,
}

async fn request_note(
    endpoint: &str,
    account: AccountId,
    faucet: AccountId,
    network: NetworkId,
    amount: Option<u64>,
) -> Result<FundingNote, FundingError> {
    let mut base =
        reqwest::Url::parse(endpoint).map_err(|error| FundingError::Invalid(error.to_string()))?;
    if !matches!(base.scheme(), "http" | "https")
        || base.query().is_some()
        || base.fragment().is_some()
    {
        return Err(FundingError::Invalid(
            "faucet_url must be an HTTP(S) base URL without query or fragment".into(),
        ));
    }
    base.set_path(&format!("{}/", base.path().trim_end_matches('/')));
    let url = |path| base.join(path).map_err(|error| FundingError::Invalid(error.to_string()));
    let http = reqwest::Client::builder();
    // Minting uses GET but changes server state. Do not retry an ambiguous response.
    #[cfg(not(target_arch = "wasm32"))]
    let http = http.retry(reqwest::retry::never());
    let http = http.build()?;
    let metadata: Metadata =
        http.get(url("get_metadata")?).send().await?.error_for_status()?.json().await?;
    let actual_id = if metadata.id.starts_with("0x") {
        AccountId::from_hex(&metadata.id)
            .map_err(|error| FundingError::Invalid(error.to_string()))?
    } else {
        let (actual_network, address) = Address::decode(&metadata.id)
            .map_err(|error| FundingError::Invalid(error.to_string()))?;
        if actual_network != network {
            return Err(FundingError::Invalid(
                "faucet address belongs to a different network".into(),
            ));
        }
        match address.id() {
            AddressId::AccountId(id) => id,
            _ => {
                return Err(FundingError::Invalid(
                    "faucet address must identify an account".into(),
                ));
            },
        }
    };
    if actual_id != faucet {
        return Err(FundingError::Invalid(format!(
            "faucet issues {actual_id}, but the chain's fee asset is {faucet}"
        )));
    }
    let amount = amount.unwrap_or(metadata.base_amount);
    if amount == 0 {
        return Err(FundingError::Invalid("faucet amount must be positive".into()));
    }
    AssetAmount::new(amount).map_err(|error| FundingError::Invalid(error.to_string()))?;
    let challenge: Challenge = http
        .get(url("pow")?)
        .query(&[("account_id", account.to_hex()), ("amount", amount.to_string())])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let nonce = solve_pow(&challenge.challenge, challenge.target).await?;
    let mint: Mint = http
        .get(url("get_tokens")?)
        .query(&[
            ("account_id", account.to_hex()),
            ("is_private_note", "false".into()),
            ("asset_amount", amount.to_string()),
            ("challenge", challenge.challenge),
            ("nonce", nonce.to_string()),
        ])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let note_id = NoteId::try_from_hex(&mint.note_id)
        .map_err(|error| FundingError::Invalid(error.to_string()))?;
    let transaction_id = crate::Word::try_from(mint.tx_id.as_str())
        .map(TransactionId::from_raw)
        .map_err(|error| FundingError::Invalid(error.to_string()))?;
    Ok(FundingNote {
        note_id,
        transaction_id,
        faucet_id: faucet,
        amount,
    })
}

async fn solve_pow(challenge: &str, target: u64) -> Result<u64, FundingError> {
    let challenge = challenge.strip_prefix("0x").unwrap_or(challenge);
    if target == 0
        || challenge.is_empty()
        || !challenge.len().is_multiple_of(2)
        || challenge.len() > 1024
    {
        return Err(FundingError::Invalid(
            "invalid faucet proof-of-work challenge or target".into(),
        ));
    }
    let bytes = challenge
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = core::str::from_utf8(pair)
                .map_err(|error| FundingError::Invalid(error.to_string()))?;
            u8::from_str_radix(pair, 16).map_err(|error| FundingError::Invalid(error.to_string()))
        })
        .collect::<Result<alloc::vec::Vec<_>, _>>()?;
    for nonce in 0..=u64::MAX {
        let hash = Sha256::hash_iter([bytes.as_slice(), &nonce.to_be_bytes()].into_iter());
        if u64::from_be_bytes(hash.as_bytes()[..8].try_into().expect("SHA-256 has eight bytes"))
            < target
        {
            return Ok(nonce);
        }
        if nonce % 1000 == 999 {
            // Yield to browser events, native tasks and the request deadline.
            sleep(Duration::from_millis(1)).await;
        }
    }
    Err(FundingError::Invalid("faucet proof of work has no solution".into()))
}

#[cfg(test)]
mod tests;

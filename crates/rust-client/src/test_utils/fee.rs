//! Funding support for running the test helpers against a fee-charging chain.

use alloc::boxed::Box;
use alloc::vec;
use core::fmt;

use anyhow::{Context, Result};
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;

use super::common::{TestClient, wait_for_tx};
use crate::note::Note;
use crate::transaction::TransactionRequestBuilder;

/// Makes accounts able to pay their own transaction fees.
#[async_trait::async_trait(?Send)]
pub trait FeeFunder: Send + Sync + fmt::Debug {
    /// Gives `account_id` enough of the chain's native fee asset to pay for its own transactions,
    /// and deploys it on-chain.
    ///
    /// `client` is the client tracking `account_id`, and is the one that must submit the deploy.
    async fn fund_and_deploy(&self, client: &mut TestClient, account_id: AccountId) -> Result<()>;
}

impl TestClient {
    /// Funds and deploys `account_id` if the chain charges fees. On a fee-free chain it does
    /// nothing, leaving the account undeployed until the test transacts with it.
    pub async fn fund_and_deploy_if_needed(&mut self, account_id: AccountId) -> Result<()> {
        if !self.chain_charges_fees().await? {
            return Ok(());
        }

        self.deploy_account(account_id).await
    }

    /// Deploys `account_id` on-chain, whether or not the chain charges fees. Already-deployed
    /// accounts are left alone, so a test need not know whether its creating helper deployed it.
    ///
    /// "Already deployed" is read off the locally tracked nonce, which only advances once a
    /// transaction of this account commits and the client syncs it. An account deployed out of
    /// band, by another client or through a note this client has not synced, still looks new here
    /// and the deploy transaction will fail rather than no-op.
    pub async fn deploy_account(&mut self, account_id: AccountId) -> Result<()> {
        let account = self
            .try_get_account(account_id)
            .await
            .with_context(|| format!("account {account_id} is not tracked by the client"))?;
        if !account.is_new() {
            return Ok(());
        }

        if self.chain_charges_fees().await? {
            let funder = self.fee_funder().cloned().context(
                "this chain charges a transaction fee, so every account a test creates has to be \
                 funded before it can transact, but this client has no fee funder. Point \
                 `MIDEN_FUNDER_ACCOUNTS` (or `--funders`) at the pre-funded wallets to draw from; \
                 `start-test-node.sh` writes them to `./data/funders`",
            )?;
            return funder.fund_and_deploy(self, account_id).await;
        }

        let request = TransactionRequestBuilder::new()
            .build()
            .context("failed to build the deploy transaction request")?;
        let tx_id = Box::pin(self.submit_new_transaction(account_id, request))
            .await
            .with_context(|| format!("failed to submit the deploy transaction of {account_id}"))?;
        wait_for_tx(self, tx_id).await.with_context(|| {
            format!("the deploy transaction of account {account_id} never committed")
        })
    }

    /// Deploys `account_id` by consuming `note`, a note carrying enough of the native fee asset for
    /// the deploy to settle its own fee.
    pub async fn deploy_by_consuming(&mut self, account_id: AccountId, note: Note) -> Result<()> {
        let note_id = note.id();

        // Consumed as an unauthenticated input, so the funder's transaction only has to have
        // reached the mempool. This doubles as the deploy, paying its fee out of the note it just
        // consumed.
        let request = TransactionRequestBuilder::new()
            .build_consume_notes(vec![note])
            .context("failed to build the funding note consumption request")?;
        let tx_id = Box::pin(self.submit_new_transaction(account_id, request)).await.with_context(
            || format!("account {account_id} failed to consume funding note {note_id}"),
        )?;

        // Waiting keeps funding invisible to the test that follows: otherwise its next
        // `sync_state` would report the deploy and the funding note as its own, which sync-summary
        // assertions pick up. It also tells a shared funder its payment landed before it releases
        // the wallet.
        wait_for_tx(self, tx_id).await.with_context(|| {
            format!("the deploy transaction of account {account_id} never committed")
        })
    }

    /// Returns whether the chain charges a non-zero fee per transaction, read from the genesis
    /// header.
    ///
    /// Exposed because a few invariants only hold fee-free: paying a fee is itself an account
    /// state change, so asserting a transaction left a commitment untouched only holds on a
    /// fee-free chain.
    ///
    /// Genesis is fetched if the store has not seen it yet, because the account-creating helpers
    /// call this before a test has necessarily synced. Reading genesis rather than the sync height
    /// assumes the test chain never revises its fee parameters, which the testing node does not.
    pub async fn chain_charges_fees(&mut self) -> Result<bool> {
        self.ensure_genesis_in_place().await?;

        let (genesis, _) = self
            .get_block_header_by_num(BlockNumber::GENESIS)
            .await?
            .context("the genesis block header is not in the client's store")?;

        Ok(genesis.fee_parameters().verification_base_fee() != 0)
    }

    /// Returns the faucet issuing the chain's native fee asset, read from the genesis header
    /// alongside [`Self::chain_charges_fees`].
    ///
    /// The faucet is configured whether or not the chain charges anything, so this is meaningful
    /// only where a fee is actually paid.
    pub async fn native_fee_faucet_id(&mut self) -> Result<AccountId> {
        self.ensure_genesis_in_place().await?;

        let (genesis, _) = self
            .get_block_header_by_num(BlockNumber::GENESIS)
            .await?
            .context("the genesis block header is not in the client's store")?;

        Ok(genesis.fee_parameters().fee_faucet_id())
    }
}

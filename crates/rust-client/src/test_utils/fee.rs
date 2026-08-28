//! Funding support for running the test helpers against a fee-charging chain.

use alloc::boxed::Box;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use anyhow::{Context, Result};
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;

use super::common::{TestClient, wait_for_tx};
use crate::note::Note;
use crate::transaction::{TransactionId, TransactionRequestBuilder};

/// Makes accounts able to pay their own transaction fees.
#[async_trait::async_trait(?Send)]
pub trait FeeFunder: Send + Sync + fmt::Debug {
    /// Gives every account in `account_ids` enough of the chain's native fee asset to pay for its
    /// own transactions, and deploys them on-chain.
    ///
    /// Takes the accounts together rather than one at a time so that a funder can pay them all in
    /// one transaction.
    ///
    /// `client` is the client tracking the accounts, and is the one that must submit the deploys.
    async fn fund_and_deploy(
        &self,
        client: &mut TestClient,
        account_ids: &[AccountId],
    ) -> Result<()>;
}

impl TestClient {
    /// Funds and deploys `account_ids` if the chain charges fees. On a fee-free chain it does
    /// nothing, leaving the accounts undeployed until the test transacts with them.
    pub async fn fund_and_deploy_if_needed(&mut self, account_ids: &[AccountId]) -> Result<()> {
        if !self.chain_charges_fees().await? {
            return Ok(());
        }

        self.deploy_accounts(account_ids).await
    }

    /// Deploys `account_id` on-chain, whether or not the chain charges fees.
    pub async fn deploy_account(&mut self, account_id: AccountId) -> Result<()> {
        self.deploy_accounts(&[account_id]).await
    }

    /// Deploys `account_ids` on-chain, whether or not the chain charges fees. Already-deployed
    /// accounts are left alone, so a test need not know whether its creating helper deployed them.
    ///
    /// Deploying several accounts at once is what lets the funder pay for them in one transaction,
    /// and lets their deploys share a single round of waiting for commitment.
    pub async fn deploy_accounts(&mut self, account_ids: &[AccountId]) -> Result<()> {
        let mut undeployed = Vec::with_capacity(account_ids.len());
        for account_id in account_ids.iter().copied() {
            let account = self
                .try_get_account(account_id)
                .await
                .with_context(|| format!("account {account_id} is not tracked by the client"))?;
            if account.is_new() {
                undeployed.push(account_id);
            }
        }
        if undeployed.is_empty() {
            return Ok(());
        }

        if self.chain_charges_fees().await? {
            let funder = self.fee_funder().cloned().context(
                "this chain charges a transaction fee, so every account a test creates has to be \
                 funded before it can transact, but this client has no fee funder. Supply the \
                 funder wallets to draw from (see the integration tests' `--funders` argument)",
            )?;
            return funder.fund_and_deploy(self, &undeployed).await;
        }

        let mut tx_ids = Vec::with_capacity(undeployed.len());
        for account_id in undeployed {
            let request = TransactionRequestBuilder::new()
                .build()
                .context("failed to build the deploy transaction request")?;
            let tx_id =
                Box::pin(self.submit_new_transaction(account_id, request)).await.with_context(
                    || format!("failed to submit the deploy transaction of {account_id}"),
                )?;
            tx_ids.push((account_id, tx_id));
        }

        self.wait_for_deploys(&tx_ids).await
    }

    /// Deploys each account by consuming the note paired with it, a note carrying enough of the
    /// native fee asset for the deploy to settle its own fee.
    pub async fn deploy_by_consuming(&mut self, funded: &[(AccountId, Note)]) -> Result<()> {
        // Every deploy is submitted before any of them is waited on, so they settle in as few
        // blocks as the node packs them into rather than one block apiece.
        let mut tx_ids = Vec::with_capacity(funded.len());
        for (account_id, note) in funded {
            let (account_id, note_id) = (*account_id, note.id());

            // Consumed as an unauthenticated input, so the funder's transaction only has to have
            // reached the mempool. This doubles as the deploy, paying its fee out of the note it
            // just consumed.
            let request = TransactionRequestBuilder::new()
                .build_consume_notes(vec![note.clone()])
                .context("failed to build the funding note consumption request")?;
            let tx_id =
                Box::pin(self.submit_new_transaction(account_id, request)).await.with_context(
                    || format!("account {account_id} failed to consume funding note {note_id}"),
                )?;
            tx_ids.push((account_id, tx_id));
        }

        self.wait_for_deploys(&tx_ids).await
    }

    /// Waits for every deploy transaction to commit.
    ///
    /// Waiting keeps funding invisible to the test that follows: otherwise its next `sync_state`
    /// would report the deploys and the funding notes as its own, which sync-summary assertions
    /// pick up. It also tells a shared funder its payment landed before it releases the wallet.
    async fn wait_for_deploys(&mut self, tx_ids: &[(AccountId, TransactionId)]) -> Result<()> {
        for (account_id, tx_id) in tx_ids.iter().copied() {
            wait_for_tx(self, tx_id).await.with_context(|| {
                format!("the deploy transaction of account {account_id} never committed")
            })?;
        }

        Ok(())
    }

    /// Returns whether the chain charges a non-zero fee per transaction, read from the genesis
    /// header.
    ///
    /// Exposed because a few invariants only hold fee-free: paying a fee is itself an account
    /// state change, so asserting a transaction left a commitment untouched only holds on a
    /// fee-free chain.
    pub async fn chain_charges_fees(&self) -> Result<bool> {
        let (genesis, _) = self
            .get_block_header_by_num(BlockNumber::GENESIS)
            .await?
            .context("the genesis block header is not in the client's store")?;

        Ok(genesis.fee_parameters().verification_base_fee() != 0)
    }
}

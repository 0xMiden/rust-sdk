//! Funding support for running the test helpers against a fee-charging chain.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use anyhow::{Context, Result};
use miden_protocol::Felt;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;

use super::common::{TestClient, wait_for_tx};
use crate::note::Note;
use crate::transaction::{TransactionId, TransactionRequestBuilder};

/// Makes accounts able to pay their own transaction fees.
#[async_trait::async_trait(?Send)]
pub trait FeeFunder: Send + Sync + fmt::Debug {
    /// Pays every account in `account_ids` enough to cover its own fees, returning each paired
    /// with the note carrying its funds.
    ///
    /// Taken together so one transaction can pay them all; returned rather than consumed so each
    /// account's own next transaction spends its note.
    async fn fund(&self, account_ids: &[AccountId]) -> Result<Vec<(AccountId, Note)>>;
}

impl TestClient {
    /// Pays `account_ids` what they need to cover their own fees, if the chain charges any.
    ///
    /// Each note is held until the account's next transaction, which consumes it and is thereby
    /// also its deploy. Does nothing on a fee-free chain.
    pub async fn fund_if_needed(&mut self, account_ids: &[AccountId]) -> Result<()> {
        if !self.chain_charges_fees().await? {
            return Ok(());
        }

        let funded = self.funder()?.fund(account_ids).await?;
        self.stash_funding(funded);

        Ok(())
    }

    /// Returns the funder, or an error naming what to supply when the chain needs one.
    fn funder(&self) -> Result<Arc<dyn FeeFunder>> {
        self.fee_funder().cloned().context(
            "this chain charges a transaction fee, so every account a test creates has to be \
             funded before it can transact, but this client has no fee funder. Supply the funder \
             wallets to draw from (see the integration tests' `--funders` argument)",
        )
    }

    /// Deploys `account_id` on-chain, whether or not the chain charges fees.
    pub async fn deploy_account(&mut self, account_id: AccountId) -> Result<()> {
        self.deploy_accounts(&[account_id]).await
    }

    /// Deploys `account_ids` on-chain, whether or not the chain charges fees. Already-deployed
    /// accounts are left alone.
    ///
    /// Taken together so the funder pays once and the deploys share a single wait.
    pub async fn deploy_accounts(&mut self, account_ids: &[AccountId]) -> Result<()> {
        let mut undeployed = Vec::with_capacity(account_ids.len());
        for account_id in account_ids.iter().copied() {
            // A zero nonce is what marks an account as never having transacted, so it reads the
            // nonce alone rather than reconstructing the account.
            let nonce =
                self.account_reader(account_id).nonce().await.with_context(|| {
                    format!("account {account_id} is not tracked by the client")
                })?;
            if nonce == Felt::ZERO {
                undeployed.push(account_id);
            }
        }
        if undeployed.is_empty() {
            return Ok(());
        }

        if self.chain_charges_fees().await? {
            // Deploying on demand means there is no later transaction to fold the funding into,
            // so the notes are consumed here.
            let mut funded = Vec::with_capacity(undeployed.len());
            for account_id in &undeployed {
                match self.take_funding(*account_id) {
                    Some(note) => funded.push((*account_id, note)),
                    None => funded.extend(self.funder()?.fund(&[*account_id]).await?),
                }
            }

            return self.deploy_by_consuming(&funded).await;
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

    /// Waits for every deploy transaction to commit, so the test that follows does not see the
    /// deploys and funding notes in its own sync.
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

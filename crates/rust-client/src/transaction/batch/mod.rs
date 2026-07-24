//! Stacks multiple transactions across one or more local accounts and submits them as one
//! proven batch via the node's `SubmitProvenBatch` endpoint.
//!
//! ## Flow
//!
//! 1. Open a builder with [`Client::new_transaction_batch`](crate::Client::new_transaction_batch).
//! 2. Add transactions via [`BatchBuilder::push`]. The first push targeting an account lazily loads
//!    its current state from the store; later pushes for that same account see the post-state of
//!    the previous push.
//! 3. Finalize with [`BatchBuilder::submit`]. This assembles a `ProposedBatch`, proves it, submits
//!    it to the node, and atomically applies the per-transaction updates to the local store.
//!
//! ## Multi-account semantics
//!
//! Each `push` specifies which local account the transaction targets. A single batch can
//! contain transactions from any combination of local accounts. Per-account in-memory state
//! stacks for repeated pushes against the same account.
//!
//! ## In-batch cross-account note flow
//!
//! A transaction in the batch may consume a note produced by an earlier transaction in the
//! same batch — even if the producer and consumer target different accounts. The user
//! extracts the expected output note from the producing request via
//! [`TransactionRequest::expected_output_own_notes`] and feeds it as an input to the
//! consuming request. Push order must respect producer-before-consumer.
//!
//! ## Constraints
//!
//! - All accounts pushed into the batch must be tracked by the client's store (otherwise the first
//!   push for that account fails with [`crate::ClientError::AccountDataNotFound`]).
//! - Locked accounts are rejected with [`crate::ClientError::AccountLocked`].
//! - No two transactions in a batch may consume the same input note (rejected with
//!   [`BatchBuilderError::DuplicateInputNote`]).
//! - The builder is succeed-only: every transaction must be pushed successfully for the batch to
//!   reach [`submit`](BatchBuilder::submit).
//!
//! ## Error semantics after RPC accept
//!
//! Once the node accepts the batch, the local store still needs to be updated. If that step
//! fails, the caller receives one of two errors that both carry the accepted `block_num`:
//!
//! - [`BatchBuilderError::BatchSubmittedButUpdateBuildFailed`] — building one of the per-tx
//!   [`TransactionStoreUpdate`]s failed.
//! - [`BatchBuilderError::BatchSubmittedButApplyFailed`] — applying the updates atomically to the
//!   local store failed.
//!
//! In both cases the recovery path is to trigger `sync_state` to reconcile.

mod data_store;
mod error;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

pub(crate) use data_store::InMemoryBatchDataStore;
pub use error::BatchBuilderError;
use miden_protocol::MIN_PROOF_SECURITY_LEVEL;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::batch::ProposedBatch;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::NoteId;
use miden_protocol::transaction::{PartialBlockchain, ProvenTransaction, TransactionInputs};
use miden_tx::auth::TransactionAuthenticator;
use miden_tx_batch::{BatchExecutor, LocalBatchProver};

use crate::store::data_store::build_partial_mmr_with_paths;
use crate::transaction::{
    TransactionRequest,
    TransactionResult,
    TransactionStoreUpdate,
    validate_executed_transaction,
};
use crate::{Client, ClientError};

/// A transaction successfully pushed into a [`BatchBuilder`]: bundles the locally-proven
/// transaction with the [`TransactionInputs`] needed by the RPC submission and the
/// [`TransactionResult`] used to build the per-tx [`TransactionStoreUpdate`].
pub(crate) struct PushedTx {
    pub(crate) proven_tx: Arc<ProvenTransaction>,
    pub(crate) transaction_inputs: TransactionInputs,
    pub(crate) tx_result: TransactionResult,
}

/// Accumulates transactions from one or more local accounts and submits them as one proven
/// batch via the node's `SubmitProvenBatch` endpoint. See the module-level docs for the full
/// usage and error semantics.
pub struct BatchBuilder<'c, AUTH> {
    pub(crate) client: &'c Client<AUTH>,
    pub(crate) data_store: InMemoryBatchDataStore,
    pub(crate) pushed_txs: Vec<PushedTx>,
    pub(crate) consumed_input_notes: BTreeSet<NoteId>,
}

impl<AUTH> BatchBuilder<'_, AUTH> {
    /// Number of successfully-pushed transactions in this batch.
    pub fn len(&self) -> usize {
        self.pushed_txs.len()
    }

    /// True if no transaction has been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.pushed_txs.is_empty()
    }
}

impl<AUTH> BatchBuilder<'_, AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Assemble the `ProposedBatch`, prove it, submit it via the client's RPC, and
    /// atomically apply the per-transaction updates to the local store.
    ///
    /// Returns the node's chain tip at submission (not the block the batch is committed). The
    /// submitted transactions are recorded locally as pending; call `sync_state` to get the block
    /// they commit in.
    pub async fn submit(self) -> Result<BlockNumber, ClientError> {
        // 1. Treat the largest ref as the reference block and the rest as authenticated. An empty
        //    batch surfaces here as a missing max.
        let ref_block_num = self
            .pushed_txs
            .iter()
            .map(|p| p.proven_tx.ref_block_num())
            .max()
            .ok_or(BatchBuilderError::Empty)?;

        let lower_refs: BTreeSet<BlockNumber> = self
            .pushed_txs
            .iter()
            .map(|p| p.proven_tx.ref_block_num())
            .filter(|&r| r < ref_block_num)
            .collect();

        let store = self.client.store.clone();

        // 2. Fetch the reference block header (from the store).
        let (ref_block_header, _) = store
            .get_block_header_by_num(ref_block_num)
            .await
            .map_err(ClientError::StoreError)?
            .ok_or_else(|| {
                ClientError::StoreError(crate::store::StoreError::BlockHeaderNotFound(
                    ref_block_num,
                ))
            })?;

        // 3. Fetch block headers for each lower ref (the ones needing authentication).
        let fetched =
            store.get_block_headers(&lower_refs).await.map_err(ClientError::StoreError)?;
        let authenticated_blocks: Vec<BlockHeader> =
            fetched.into_iter().map(|(header, _)| header).collect();
        let fetched_nums: BTreeSet<BlockNumber> =
            authenticated_blocks.iter().map(BlockHeader::block_num).collect();
        if let Some(&missing) = lower_refs.difference(&fetched_nums).next() {
            return Err(ClientError::StoreError(crate::store::StoreError::BlockHeaderNotFound(
                missing,
            )));
        }

        // 4. Build PartialMmr + PartialBlockchain using the current blockchain peaks — this matches
        //    the MMR convention used by `ClientDataStore::get_transaction_inputs`.
        let current_peaks =
            store.get_current_blockchain_peaks().await.map_err(ClientError::StoreError)?;
        let partial_mmr =
            build_partial_mmr_with_paths(&store, current_peaks, &authenticated_blocks).await?;
        let partial_blockchain = PartialBlockchain::new(partial_mmr, authenticated_blocks)?;

        // 5. Split pushed_txs into the three views required by the remaining steps and build the
        //    ProposedBatch.
        let len = self.pushed_txs.len();
        let mut proven_txs: Vec<Arc<ProvenTransaction>> = Vec::with_capacity(len);
        let mut transaction_inputs: Vec<TransactionInputs> = Vec::with_capacity(len);
        let mut tx_results: Vec<TransactionResult> = Vec::with_capacity(len);
        for pushed in self.pushed_txs {
            proven_txs.push(pushed.proven_tx);
            transaction_inputs.push(pushed.transaction_inputs);
            tx_results.push(pushed.tx_result);
        }

        // TODO: field is left unused as of now because all txs in batch are already proven.
        // This will be populated once a feature like remote proving in batches is implemented.
        let unauthenticated_note_proofs = BTreeMap::new();
        let proposed_batch = ProposedBatch::new(
            proven_txs,
            ref_block_header,
            partial_blockchain,
            unauthenticated_note_proofs,
            MIN_PROOF_SECURITY_LEVEL,
        )?;

        // 6. Execute the batch kernel, then prove synchronously.
        let executed_batch = BatchExecutor::new().execute(proposed_batch.clone())?;
        let proven_batch = LocalBatchProver::new().prove(executed_batch)?;

        // 7. Submit via RPC.
        let mut updates: Vec<TransactionStoreUpdate> = Vec::with_capacity(len);
        let block_num = self
            .client
            .rpc_api
            .submit_proven_batch(proven_batch, proposed_batch, transaction_inputs)
            .await?;

        // 8. Build per-tx TransactionStoreUpdates.
        for tx_result in &tx_results {
            let update =
                self.client.get_transaction_store_update(tx_result, block_num).await.map_err(
                    |source| BatchBuilderError::BatchSubmittedButUpdateBuildFailed {
                        block_num,
                        source,
                    },
                )?;
            updates.push(update);
        }

        // 9. Apply atomically; if it fails, return BatchSubmittedButApplyFailed.
        if let Err(source) = self.client.store.apply_transaction_batch(updates).await {
            return Err(ClientError::from(BatchBuilderError::BatchSubmittedButApplyFailed {
                block_num,
                source,
            }));
        }

        Ok(block_num)
    }

    /// Execute `req` against the batch's in-memory state for `account_id`, prove it using
    /// the client's configured prover, and append the resulting proven transaction to the
    /// batch. The first push for a given account lazily loads its state from the store.
    ///
    /// Consumes the builder and returns it on success. On failure the builder is dropped
    /// along with every transaction accumulated so far; the caller cannot recover the
    /// partial batch.
    pub async fn push(
        mut self,
        account_id: AccountId,
        req: TransactionRequest,
    ) -> Result<Self, ClientError> {
        // 1. Dedup input notes globally for the batch.
        for note_id in req.input_note_ids() {
            if self.consumed_input_notes.contains(&note_id) {
                return Err(ClientError::from(BatchBuilderError::DuplicateInputNote(note_id)));
            }
        }

        // 2. Execute against in-batch state, prove.
        let tx_result =
            execute_transaction_for_batch(self.client, &mut self.data_store, account_id, req)
                .await?;
        let tx_inputs = tx_result.executed_transaction().tx_inputs().clone();
        let proven_tx = self.client.prove_transaction(&tx_result).await?;

        // 3. Record consumed input notes, append PushedTx.
        for note in tx_result.consumed_notes().iter() {
            self.consumed_input_notes.insert(note.id());
        }
        self.pushed_txs.push(PushedTx {
            proven_tx: Arc::new(proven_tx),
            transaction_inputs: tx_inputs,
            tx_result,
        });
        Ok(self)
    }
}

/// Executes a single transaction, that is part of the batch to be sent to the node.
/// Transaction is ran as the provided `Account`
async fn execute_transaction_for_batch<AUTH>(
    client: &Client<AUTH>,
    data_store: &mut InMemoryBatchDataStore,
    account_id: AccountId,
    transaction_request: TransactionRequest,
) -> Result<TransactionResult, ClientError>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    let mut account = if let Some(account) = data_store.get_account(account_id) {
        account.clone()
    } else {
        let record = client
            .store
            .get_account(account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(account_id))?;
        if record.is_locked() {
            return Err(ClientError::AccountLocked(account_id));
        }
        let account: Account = record.try_into()?;
        account
    };

    let account_id = account.id();
    let prep = client.prepare_transaction(&account, transaction_request).await?;

    data_store.register_note_scripts(prep.output_note_scripts());
    for fpi_account in &prep.foreign_account_inputs {
        data_store.mast_store().load_account_code(fpi_account.code());
    }
    data_store.register_foreign_account_inputs(prep.foreign_account_inputs);

    data_store.mast_store().load_account_code(account.code());

    let mut notes = prep.notes;
    if prep.ignore_invalid_notes {
        notes = client
            .get_valid_input_notes(&account, notes, prep.tx_args.clone(), &prep.output_recipients)
            .await?;
    }

    let executed_transaction = client
        .build_executor(data_store)?
        .execute_transaction(account_id, prep.block_num, notes, prep.tx_args)
        .await?;

    // Cache new account state in memory data store.
    let patch = executed_transaction.account_patch();
    let account = if patch.is_full_state() {
        Account::try_from(patch).map_err(ClientError::AccountError)?
    } else {
        account.apply_patch(patch).map_err(ClientError::AccountError)?;
        account
    };
    data_store.cache_account(account_id, account);

    validate_executed_transaction(&executed_transaction, &prep.output_recipients)?;
    TransactionResult::new(executed_transaction, prep.future_notes)
}

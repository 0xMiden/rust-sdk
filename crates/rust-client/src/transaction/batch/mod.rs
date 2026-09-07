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
//! - A failed [`push`](BatchBuilder::push) leaves the batch exactly as it was, so the caller may
//!   retry with a different request or submit the transactions accumulated so far.
//!
//! ## Error semantics around submission
//!
//! A submission that comes back without a definite outcome raises
//! [`BatchBuilderError::BatchSubmissionOutcomeUnknown`]. The node may or may not have accepted the
//! batch and nothing was recorded locally, so the error carries a [`ProvenBatchSubmission`] to
//! resend with [`Client::retry_proven_batch`].
//!
//! Once the node accepts the batch, the local store still needs to be updated. If that step
//! fails, the caller receives one of two errors that both carry the accepted `block_num`:
//!
//! - [`BatchBuilderError::BatchSubmittedButUpdateBuildFailed`] — building one of the per-tx
//!   [`TransactionStoreUpdate`]s failed.
//! - [`BatchBuilderError::BatchSubmittedButApplyFailed`] — applying the updates atomically to the
//!   local store failed.
//!
//! In all three cases `sync_state` reconciles the accounts with what the network holds. It does
//! not create transaction records, though: syncing updates records the client already holds and
//! never inserts missing ones. For the unknown outcome an accepted retry writes them; for the two
//! post-accept errors nothing will, since neither carries the updates that failed.

mod data_store;
mod error;
mod staged_smt;

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::sync::Arc;
use alloc::vec::Vec;

pub(crate) use data_store::InMemoryBatchDataStore;
pub use error::BatchBuilderError;
use miden_protocol::MIN_PROOF_SECURITY_LEVEL;
use miden_protocol::account::AccountId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::NoteId;
use miden_protocol::transaction::{PartialBlockchain, ProvenTransaction, TransactionId};
use miden_tx::auth::TransactionAuthenticator;
use miden_tx_batch::{BatchExecutor, LocalBatchProver};

use crate::rpc::RpcError;
use crate::rpc::encryption::seal_transaction_inputs;
use crate::store::data_store::{ClientDataStore, build_partial_mmr_with_paths};
use crate::transaction::{
    TransactionRequest,
    TransactionResult,
    TransactionStoreUpdate,
    validate_executed_transaction,
};
use crate::{Client, ClientError};

/// A proven batch together with everything else its submission needs, so a submission whose
/// outcome the node never confirmed can be retried without executing or proving again.
///
/// Handed back by [`BatchBuilderError::BatchSubmissionOutcomeUnknown`] and accepted by
/// [`Client::retry_proven_batch`].
#[derive(Debug, Clone)]
pub struct ProvenBatchSubmission {
    proven_batch: ProvenBatch,
    proposed_batch: Box<ProposedBatch>,
    /// The validator set's key can rotate between attempts, so a retry has to seal these again,
    /// and `BatchBuilder::submit` needs the whole results after the RPC for the store updates.
    tx_results: Vec<TransactionResult>,
}

impl ProvenBatchSubmission {
    /// Number of transactions in the batch.
    pub fn transaction_count(&self) -> usize {
        self.tx_results.len()
    }

    /// Ids the batch was submitted with. Nothing is recorded for them yet, so they reach
    /// `get_transactions` only once a retry is accepted.
    pub fn transaction_ids(&self) -> impl Iterator<Item = TransactionId> + '_ {
        self.tx_results.iter().map(|tx_result| tx_result.executed_transaction().id())
    }
}

/// A transaction successfully pushed into a [`BatchBuilder`]: the locally-proven transaction
/// alongside the [`TransactionResult`] used to build the per-tx [`TransactionStoreUpdate`]. The
/// transaction inputs the RPC submission seals are read back from the result.
pub(crate) struct PushedTx {
    pub(crate) proven_tx: Arc<ProvenTransaction>,
    pub(crate) tx_result: TransactionResult,
}

/// Accumulates transactions from one or more local accounts and submits them as one proven
/// batch via the node's `SubmitProvenBatch` endpoint. See the module-level docs for the full
/// usage and error semantics.
pub struct BatchBuilder<'c, AUTH> {
    pub(crate) client: &'c mut Client<AUTH>,
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

impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Open a new [`BatchBuilder`] for accumulating transactions across one or more local
    /// accounts.
    ///
    /// See the module-level docs for usage and constraints.
    pub fn new_transaction_batch(&mut self) -> BatchBuilder<'_, AUTH> {
        let inner_data_store = ClientDataStore::new(self.store.clone(), self.rpc_api.clone());
        BatchBuilder {
            client: self,
            data_store: InMemoryBatchDataStore::new(inner_data_store),
            pushed_txs: Vec::new(),
            consumed_input_notes: BTreeSet::new(),
        }
    }

    /// Resubmits an already-proven batch and returns the node's chain tip upon mempool admission.
    ///
    /// This is the retry entry point for a submission whose outcome was never confirmed: pass back
    /// the [`ProvenBatchSubmission`] carried by
    /// [`BatchBuilderError::BatchSubmissionOutcomeUnknown`] and the batch goes out again without
    /// being executed or proven a second time. The batch id is fixed, so resending it cannot
    /// duplicate its effects, but the node rejects it as a conflict if the original did land.
    ///
    /// That error is the only source of a [`ProvenBatchSubmission`]: the type has no public
    /// constructor, and assembling and proving a batch goes through [`BatchBuilder`].
    ///
    /// A retry the node accepts records the batch the way the first send would have, so the
    /// transactions reach the store no matter which attempt landed. A retry the node rejects
    /// records nothing, and neither will a later sync: syncing updates records the client already
    /// holds and never inserts missing ones.
    ///
    /// # Errors
    ///
    /// Returns [`BatchBuilderError::BatchSubmissionOutcomeUnknown`] when the submission comes back
    /// without a definite answer. Every other failure is a rejection the node issued deliberately.
    pub async fn retry_proven_batch(
        &mut self,
        submission: &ProvenBatchSubmission,
    ) -> Result<BlockNumber, ClientError> {
        self.send_and_apply_proven_batch(submission).await
    }

    /// Seals the submission's inputs against the current key, sends the batch, and on acceptance
    /// applies the per-transaction store updates atomically.
    ///
    /// Shared by the first send from [`BatchBuilder::submit`] and by every retry through
    /// [`Client::retry_proven_batch`], so both record what the node took and both map an
    /// unconfirmed outcome to the error that carries the submission back.
    async fn send_and_apply_proven_batch(
        &mut self,
        submission: &ProvenBatchSubmission,
    ) -> Result<BlockNumber, ClientError> {
        // Each entry is sealed against its own transaction id, with fresh randomness per attempt.
        let key = self.transaction_encryption_key().await?;
        let sealed_inputs = submission
            .tx_results
            .iter()
            .map(|tx_result| {
                let executed = tx_result.executed_transaction();
                seal_transaction_inputs(&mut self.rng, &key, executed.id(), executed.tx_inputs())
            })
            .collect::<Result<Vec<_>, _>>()?;

        let result = self
            .rpc_api
            .submit_proven_batch(
                submission.proven_batch.clone(),
                (*submission.proposed_batch).clone(),
                sealed_inputs,
            )
            .await;
        if let Err(err) = &result {
            self.forget_stale_transaction_encryption_key(err).await;
        }

        let block_num = result.map_err(|err| promote_indeterminate_submission(err, submission))?;

        // The node took the batch. Record it, one update per transaction, applied atomically.
        let mut updates: Vec<TransactionStoreUpdate> =
            Vec::with_capacity(submission.transaction_count());
        for tx_result in &submission.tx_results {
            let update = self.get_transaction_store_update(tx_result, block_num).await.map_err(
                |source| BatchBuilderError::BatchSubmittedButUpdateBuildFailed {
                    block_num,
                    source,
                },
            )?;
            updates.push(update);
        }

        if let Err(source) = self.store.apply_transaction_batch(updates).await {
            return Err(ClientError::from(BatchBuilderError::BatchSubmittedButApplyFailed {
                block_num,
                source,
            }));
        }

        Ok(block_num)
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

        // 5. Split pushed_txs into the two views required by the remaining steps and build the
        //    ProposedBatch.
        let len = self.pushed_txs.len();
        let mut proven_txs: Vec<Arc<ProvenTransaction>> = Vec::with_capacity(len);
        let mut tx_results: Vec<TransactionResult> = Vec::with_capacity(len);
        for pushed in self.pushed_txs {
            proven_txs.push(pushed.proven_tx);
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

        // 7. Submit via RPC and record what the node took. The proven batch is kept so an
        //    unconfirmed submission can be retried without executing or proving again.
        let submission = ProvenBatchSubmission {
            proven_batch,
            proposed_batch: Box::new(proposed_batch),
            tx_results,
        };
        let block_num = self.client.send_and_apply_proven_batch(&submission).await?;

        Ok(block_num)
    }

    /// Execute `req` against the batch's in-memory state for `account_id`, prove it using
    /// the client's configured prover, and append the resulting proven transaction to the
    /// batch. The first push for a given account lazily loads its state from the store.
    ///
    /// The batch is only advanced once the transaction has both executed and been proven, so on
    /// failure the builder still holds exactly the transactions it held before the call and
    /// remains usable. Returns `&mut Self` so pushes can be chained.
    pub async fn push(
        &mut self,
        account_id: AccountId,
        req: TransactionRequest,
    ) -> Result<&mut Self, ClientError> {
        // 1. Dedup input notes globally for the batch.
        for note_id in req.input_note_ids() {
            if self.consumed_input_notes.contains(&note_id) {
                return Err(ClientError::from(BatchBuilderError::DuplicateInputNote(note_id)));
            }
        }

        // 2. Execute against in-batch state, then prove. Both run before any batch state is
        //    advanced, so a failure in either leaves the builder untouched. Execution holds a large
        //    future, boxed here so callers don't have to.
        let tx_result =
            Box::pin(execute_transaction_for_batch(self.client, &self.data_store, account_id, req))
                .await?;
        let proven_tx = self.client.prove_transaction(&tx_result).await?;

        // 3. The transaction is final: fold it into the in-batch account state, record its consumed
        //    notes, and append it to the batch.
        self.data_store
            .apply_executed_transaction(tx_result.executed_transaction())
            .await?;
        for note in tx_result.consumed_notes().iter() {
            self.consumed_input_notes.insert(note.id());
        }
        self.pushed_txs.push(PushedTx {
            proven_tx: Arc::new(proven_tx),
            tx_result,
        });
        Ok(self)
    }
}

/// Executes a single transaction that is part of the batch to be sent to the node.
/// The transaction runs against the current in-batch partial account state.
async fn execute_transaction_for_batch<AUTH>(
    client: &Client<AUTH>,
    data_store: &InMemoryBatchDataStore,
    account_id: AccountId,
    transaction_request: TransactionRequest,
) -> Result<TransactionResult, ClientError>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    let account_reader = client.account_reader(account_id);
    if account_reader.status().await?.is_locked() {
        return Err(ClientError::AccountLocked(account_id));
    }

    let account = match data_store.cached_account(account_id) {
        Some(account) => account,
        None => account_reader.partial_account().await?,
    };

    let prep = client.prepare_transaction_for_batch(&account, transaction_request).await?;

    data_store.register_note_scripts(prep.output_note_scripts());
    for fpi_account in &prep.foreign_account_inputs {
        data_store.mast_store().load_account_code(fpi_account.code());
    }
    data_store.register_foreign_account_inputs(prep.foreign_account_inputs);

    data_store.mast_store().load_account_code(account.code());

    let mut notes = prep.notes;
    if prep.ignore_invalid_notes {
        notes = client
            .get_valid_input_notes(
                data_store,
                account_id,
                prep.block_num,
                notes,
                prep.tx_args.clone(),
            )
            .await?;
    }

    let executed_transaction = client
        .build_executor(data_store)?
        .execute_transaction(account_id, prep.block_num, notes, prep.tx_args)
        .await?;

    validate_executed_transaction(&executed_transaction, &prep.output_recipients)?;
    TransactionResult::new(executed_transaction, prep.future_notes)
}

/// Promotes a batch submission failure whose outcome is unknown, attaching everything a retry
/// needs. Any other failure is a rejection the node issued deliberately and passes through
/// unchanged.
fn promote_indeterminate_submission(
    err: RpcError,
    submission: &ProvenBatchSubmission,
) -> ClientError {
    if !err.is_indeterminate_submission() {
        return ClientError::RpcError(err);
    }

    BatchBuilderError::BatchSubmissionOutcomeUnknown {
        submission: Box::new(submission.clone()),
        source: err,
    }
    .into()
}

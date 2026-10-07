//! Stacks multiple transactions across one or more local accounts and submits them as one proven
//! batch via the node's `SubmitProvenBatch` endpoint.
//!
//! ## Flow
//!
//! 1. Open a batch with [`Client::new_transaction_batch`].
//! 2. Add transactions via [`Client::push_to_batch`]. The first push targeting an account lazily
//!    loads its current state from the store; later pushes for that same account see the
//!    post-state of the previous push.
//! 3. Finalize with [`Client::submit_transaction_batch`]. This assembles a `ProposedBatch`, proves
//!    it, submits it to the node, and atomically applies one [`BatchStoreUpdate`] to the local
//!    store.
//!
//! ## Multi-account semantics
//!
//! Each `push` specifies which local account the transaction targets. A single batch can contain
//! transactions from any combination of local accounts. Per-account in-memory state stacks for
//! repeated pushes against the same account.
//!
//! ## In-batch cross-account note flow
//!
//! A transaction in the batch may consume a note produced by an earlier transaction in the same
//! batch — even if the producer and consumer target different accounts. The user extracts the
//! expected output note from the producing request via
//! [`TransactionRequest::expected_output_own_notes`] and feeds it as an input to the consuming
//! request. Push order must respect producer-before-consumer.
//!
//! ## Transactions proven by other parties
//!
//! [`BatchBuilder::push_proven_transaction`] adds a transaction that another party executed and
//! proved. The client does not have to track its account and does not record it locally. Do not
//! also push a transaction of this client for the same account, because the node then rejects the
//! batch.
//!
//! ## Constraints
//!
//! - All accounts pushed via [`Client::push_to_batch`] must be tracked by the client's store
//!   (otherwise the first push for that account fails with
//!   [`crate::ClientError::AccountDataNotFound`]).
//! - Locked accounts are rejected with [`crate::ClientError::AccountLocked`].
//! - No two transactions in a batch may consume the same input note (rejected with
//!   [`BatchBuilderError::DuplicateInputNote`]).
//! - A failed [`Client::push_to_batch`] leaves the batch exactly as it was, so the caller may retry
//!   with a different request or submit the transactions accumulated so far.
//! - The batch builds on the state each account had at its first push. If another transaction for
//!   one of these accounts reaches the node first, the node rejects the batch.
//! - The batch references the client's sync height, so no transaction may reference a later block.
//!
//! ## Account allowlist
//!
//! [`Client::submit_transaction_batch`] asks the network allowlist about each account that the
//! batch creates before the batch is proven. It fails with
//! [`crate::ClientError::AccountNotAllowlisted`] if the network does not accept one of them.
//! [`Client::retry_proven_batch`] does not ask again.
//!
//! ## Error semantics around submission
//!
//! A submission that comes back without a definite outcome raises
//! [`BatchBuilderError::BatchSubmissionOutcomeUnknown`]. The node may or may not have accepted the
//! batch and nothing was recorded locally, so the error carries a [`ProvenBatchSubmission`] to
//! resend with [`Client::retry_proven_batch`].
//!
//! Once the node accepts the batch, the local store still needs to be updated. If that step fails,
//! the caller receives one of two errors that both carry the accepted `block_num`:
//!
//! - [`BatchBuilderError::BatchSubmittedButUpdateBuildFailed`] — building the [`BatchStoreUpdate`]
//!   failed.
//! - [`BatchBuilderError::BatchSubmittedButApplyFailed`] — applying the update atomically to the
//!   local store failed. The error carries the update, so the caller can apply it again with
//!   [`Client::apply_batch_update`].
//!
//! In all three cases `sync_state` reconciles the accounts with what the network holds. It does not
//! create transaction records, though: syncing updates records the client already holds and never
//! inserts missing ones. For the unknown outcome an accepted retry writes them. For an apply
//! failure the carried update writes them. For an update build failure nothing will.

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
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteId, Nullifier};
use miden_protocol::transaction::{
    InputNoteCommitment,
    ProvenTransaction,
    TransactionId,
    TransactionInputs,
    TransactionVerifier,
};
use miden_tx::auth::TransactionAuthenticator;
use miden_tx_batch::{BatchExecutor, LocalBatchProver};

use crate::note::NoteUpdateTracker;
use crate::rpc::RpcError;
use crate::rpc::encryption::seal_transaction_inputs;
use crate::store::data_store::ClientDataStore;
use crate::transaction::{
    BatchStoreUpdate,
    TransactionRequest,
    TransactionResult,
    TransactionStoreUpdateError,
    ensure_account_allowed,
    validate_executed_transaction,
};
use crate::{Client, ClientError};

/// A proven batch together with everything else its submission needs, so a submission whose outcome
/// the node never confirmed can be retried without executing or proving again.
///
/// Handed back by [`BatchBuilderError::BatchSubmissionOutcomeUnknown`] and accepted by
/// [`Client::retry_proven_batch`].
#[derive(Debug, Clone)]
pub struct ProvenBatchSubmission {
    proven_batch: ProvenBatch,
    proposed_batch: Box<ProposedBatch>,
    /// One entry per transaction, in batch order. The validator set's key can rotate between
    /// attempts, so a retry has to seal these again.
    tx_inputs: Vec<TransactionInputs>,
    /// The results of the transactions that this client executed.
    /// `Client::submit_transaction_batch` needs them after the RPC for the store update.
    tx_results: Vec<TransactionResult>,
}

impl ProvenBatchSubmission {
    /// Number of transactions in the batch.
    pub fn transaction_count(&self) -> usize {
        self.tx_inputs.len()
    }

    /// Ids the batch was submitted with. Nothing is recorded for them yet, so they reach
    /// `get_transactions` only once a retry is accepted.
    pub fn transaction_ids(&self) -> impl Iterator<Item = TransactionId> + '_ {
        self.proposed_batch.transactions().iter().map(|proven_tx| proven_tx.id())
    }
}

/// A transaction successfully pushed into a [`BatchBuilder`]: the proven transaction alongside the
/// transaction inputs that the RPC submission seals.
pub(crate) struct PushedTx {
    pub(crate) proven_tx: Arc<ProvenTransaction>,
    pub(crate) tx_inputs: TransactionInputs,
}

/// Accumulates transactions from one or more local accounts. [`Client::push_to_batch`] adds
/// transactions and [`Client::submit_transaction_batch`] submits them as one proven batch via the
/// node's `SubmitProvenBatch` endpoint. See the module-level docs for the full usage and error
/// semantics.
pub struct BatchBuilder {
    pub(crate) data_store: InMemoryBatchDataStore,
    pub(crate) pushed_txs: Vec<PushedTx>,
    pub(crate) tx_results: Vec<TransactionResult>,
    pub(crate) consumed_input_notes: BTreeSet<NoteId>,
}

impl BatchBuilder {
    /// Number of successfully-pushed transactions in this batch.
    pub fn len(&self) -> usize {
        self.pushed_txs.len()
    }

    /// True if no transaction has been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.pushed_txs.is_empty()
    }

    /// Appends a transaction that another party executed and proved. The node needs `tx_inputs`,
    /// the inputs that the transaction executed with, and the proven transaction does not carry
    /// them. A failed push leaves the batch exactly as it was.
    ///
    /// # Errors
    ///
    /// - Returns [`BatchBuilderError::DuplicateNullifier`] if an earlier transaction in the batch
    ///   consumes one of the input notes.
    /// - Returns [`BatchBuilderError::InvalidTransactionProof`] if the proof does not verify.
    pub fn push_proven_transaction(
        &mut self,
        proven_tx: ProvenTransaction,
        tx_inputs: impl Into<TransactionInputs>,
    ) -> Result<(), BatchBuilderError> {
        let consumed: BTreeSet<Nullifier> = self
            .pushed_txs
            .iter()
            .flat_map(|pushed| {
                pushed.proven_tx.input_notes().iter().map(InputNoteCommitment::nullifier)
            })
            .collect();
        if let Some(note) =
            proven_tx.input_notes().iter().find(|note| consumed.contains(&note.nullifier()))
        {
            return Err(BatchBuilderError::DuplicateNullifier(note.nullifier()));
        }

        // The batch prover settles any precompile obligation that the outcome carries.
        let _outcome = TransactionVerifier::new(MIN_PROOF_SECURITY_LEVEL)
            .verify(&proven_tx)
            .map_err(|source| BatchBuilderError::InvalidTransactionProof {
                tx_id: proven_tx.id(),
                source,
            })?;

        self.pushed_txs.push(PushedTx {
            proven_tx: Arc::new(proven_tx),
            tx_inputs: tx_inputs.into(),
        });
        Ok(())
    }
}

impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Open a new [`BatchBuilder`] for accumulating transactions across one or more local accounts.
    ///
    /// See the module-level docs for usage and constraints.
    pub fn new_transaction_batch(&self) -> BatchBuilder {
        let inner_data_store = ClientDataStore::new(self.store.clone(), self.rpc_api.clone());
        BatchBuilder {
            data_store: InMemoryBatchDataStore::new(inner_data_store),
            pushed_txs: Vec::new(),
            tx_results: Vec::new(),
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
    /// applies the batch update atomically.
    ///
    /// Shared by the first send from [`Client::submit_transaction_batch`] and by every retry
    /// through [`Client::retry_proven_batch`], so both record what the node took and both map an
    /// unconfirmed outcome to the error that carries the submission back.
    async fn send_and_apply_proven_batch(
        &mut self,
        submission: &ProvenBatchSubmission,
    ) -> Result<BlockNumber, ClientError> {
        // Each entry is sealed against its own transaction id, with fresh randomness per attempt.
        let key = self.transaction_encryption_key().await?;
        let sealed_inputs = submission
            .proposed_batch
            .transactions()
            .iter()
            .zip(&submission.tx_inputs)
            .map(|(proven_tx, tx_inputs)| {
                seal_transaction_inputs(&mut self.rng, &key, proven_tx.id(), tx_inputs)
            })
            .collect::<Result<Vec<_>, _>>()?;

        let result = self
            .rpc_api
            .submit_proven_batch(
                &submission.proven_batch,
                &submission.proposed_batch,
                sealed_inputs,
            )
            .await;
        if let Err(err) = &result {
            self.forget_stale_transaction_encryption_key(err).await;
        }

        let block_num = result.map_err(|err| promote_indeterminate_submission(err, submission))?;

        // The node took the batch. Record it as one update, applied atomically.
        let batch_update = self
            .get_batch_store_update(&submission.tx_results, block_num)
            .await
            .map_err(|source| BatchBuilderError::BatchSubmittedButUpdateBuildFailed {
                block_num,
                source,
            })?;

        if let Err(source) = self.store.apply_transaction_batch(batch_update.clone()).await {
            return Err(ClientError::from(BatchBuilderError::BatchSubmittedButApplyFailed {
                block_num,
                pending_update: Box::new(batch_update),
                source,
            }));
        }

        // Observer failures are logged and never propagate, as in `submit_new_transaction`.
        for tx_result in &submission.tx_results {
            for observer in &self.transaction_observers {
                crate::errors::log_observer_failure(
                    observer.name(),
                    "TransactionObserver::apply",
                    observer.apply(tx_result).await,
                );
            }
        }

        Ok(block_num)
    }

    /// Builds a [`BatchStoreUpdate`] for the transactions of a submitted batch at the specified
    /// submission height.
    ///
    /// The note updates of the transactions are merged in batch order, so a note that one
    /// transaction creates and a later transaction consumes is stored once, in its consumed state.
    pub async fn get_batch_store_update(
        &self,
        tx_results: &[TransactionResult],
        submission_height: BlockNumber,
    ) -> Result<BatchStoreUpdate, TransactionStoreUpdateError> {
        let mut note_updates = NoteUpdateTracker::default();
        let mut new_tags = Vec::new();
        for tx_result in tx_results {
            let tx_update = self.get_transaction_store_update(tx_result, submission_height).await?;
            note_updates.merge(tx_update.note_updates().clone());
            new_tags.extend_from_slice(tx_update.new_tags());
        }

        Ok(BatchStoreUpdate::new(
            tx_results
                .iter()
                .map(|tx_result| tx_result.executed_transaction().clone())
                .collect(),
            submission_height,
            note_updates,
            new_tags,
        ))
    }

    /// Persists the result of a submitted transaction batch into the local store. This is the batch
    /// counterpart of [`Client::apply_transaction_update`].
    pub async fn apply_batch_update(
        &self,
        batch_update: BatchStoreUpdate,
    ) -> Result<(), ClientError> {
        Ok(self.store.apply_transaction_batch(batch_update).await?)
    }
}

impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Assemble the `ProposedBatch` from `batch`, prove it, submit it via the client's RPC, and
    /// atomically apply the batch update to the local store.
    ///
    /// Returns the node's chain tip at submission (not the block the batch is committed). The
    /// submitted transactions are recorded locally as pending; call `sync_state` to get the block
    /// they commit in.
    pub async fn submit_transaction_batch(
        &mut self,
        batch: BatchBuilder,
    ) -> Result<BlockNumber, ClientError> {
        if batch.is_empty() {
            return Err(BatchBuilderError::Empty.into());
        }

        // Accounts that the batch creates are gated by the network allowlist. Ask before the batch
        // is proven.
        let account_ids: BTreeSet<AccountId> =
            batch.pushed_txs.iter().map(|p| p.proven_tx.account_id()).collect();
        for account_id in account_ids {
            if self.is_allowlist_gated(account_id).await? {
                ensure_account_allowed(account_id, self.is_account_allowed(account_id).await)?;
            }
        }

        // 1. Anchor the batch at the sync height, because the partial blockchain comes from the
        //    current peaks. The lower reference blocks are authenticated against them.
        let ref_blocks: BTreeSet<BlockNumber> =
            batch.pushed_txs.iter().map(|p| p.proven_tx.ref_block_num()).collect();
        let (ref_block_header, partial_blockchain) =
            self.chain_anchor_at_tip(ref_blocks).await?.into_parts();

        // 2. Split pushed_txs into the two views required by the remaining steps and build the
        //    ProposedBatch.
        let (proven_txs, tx_inputs): (Vec<_>, Vec<_>) =
            batch.pushed_txs.into_iter().map(|p| (p.proven_tx, p.tx_inputs)).unzip();

        // TODO: field is left unused as of now because all txs in batch are already proven. This
        // will be populated once a feature like remote proving in batches is implemented.
        let unauthenticated_note_proofs = BTreeMap::new();
        let proposed_batch = ProposedBatch::new(
            proven_txs,
            ref_block_header,
            partial_blockchain,
            unauthenticated_note_proofs,
            MIN_PROOF_SECURITY_LEVEL,
        )?;

        // 3. Execute the batch kernel, then prove synchronously.
        let executed_batch = BatchExecutor::new().execute(proposed_batch.clone())?;
        let proven_batch =
            LocalBatchProver::new(miden_tx::Prover::default()).prove(executed_batch)?;

        // 4. Submit via RPC and record what the node took. The proven batch is kept so an
        //    unconfirmed submission can be retried without executing or proving again.
        let submission = ProvenBatchSubmission {
            proven_batch,
            proposed_batch: Box::new(proposed_batch),
            tx_inputs,
            tx_results: batch.tx_results,
        };
        let block_num = self.send_and_apply_proven_batch(&submission).await?;

        Ok(block_num)
    }

    /// Execute `req` against the in-memory state of `batch` for `account_id`, prove it using the
    /// client's configured prover, and append the resulting proven transaction to `batch`. The
    /// first push for a given account lazily loads its state from the store.
    ///
    /// The batch is only advanced once the transaction has both executed and been proven, so on
    /// failure `batch` still holds exactly the transactions it held before the call and remains
    /// usable. Push only to a batch that this client opened with [`Client::new_transaction_batch`].
    pub async fn push_to_batch(
        &self,
        batch: &mut BatchBuilder,
        account_id: AccountId,
        req: TransactionRequest,
    ) -> Result<(), ClientError> {
        // 1. Dedup input notes globally for the batch.
        for note_id in req.input_note_ids() {
            if batch.consumed_input_notes.contains(&note_id) {
                return Err(ClientError::from(BatchBuilderError::DuplicateInputNote(note_id)));
            }
        }

        // 2. Execute against in-batch state, then prove. Both run before any batch state is
        //    advanced, so a failure in either leaves the batch untouched. Execution holds a large
        //    future, boxed here so callers don't have to.
        let tx_result =
            Box::pin(execute_transaction_for_batch(self, &batch.data_store, account_id, req))
                .await?;

        let proven_tx = self.prove_transaction(&tx_result).await?;

        // 3. The transaction is final: fold it into the in-batch account state, record its consumed
        //    notes, and append it to the batch.
        batch
            .data_store
            .apply_executed_transaction(tx_result.executed_transaction())
            .await?;
        for note in tx_result.consumed_notes().iter() {
            batch.consumed_input_notes.insert(note.id());
        }
        batch.pushed_txs.push(PushedTx {
            proven_tx: Arc::new(proven_tx),
            tx_inputs: tx_result.executed_transaction().tx_inputs().clone(),
        });
        batch.tx_results.push(tx_result);
        Ok(())
    }
}

/// Executes a single transaction that is part of the batch to be sent to the node. The transaction
/// runs against the current in-batch partial account state.
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
    data_store.register_block_numbers(prep.block_numbers.iter().copied());
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
    Ok(TransactionResult::new(executed_transaction, prep.future_notes))
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

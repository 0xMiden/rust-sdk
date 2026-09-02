//! Provides APIs for creating, executing, proving, and submitting transactions to the Miden
//! network.
//!
//! ## Overview
//!
//! This module enables clients to:
//!
//! - Build transaction requests using the [`TransactionRequestBuilder`].
//!   - [`TransactionRequestBuilder`] contains simple builders for standard transaction types, such
//!     as `p2id` (pay-to-id)
//! - Execute transactions via the local transaction executor and generate a [`TransactionResult`]
//!   that includes execution details and relevant notes for state tracking.
//! - Prove transactions (locally or remotely) using a [`TransactionProver`] and submit the proven
//!   transactions to the network.
//! - Track and update the state of transactions, including their status (e.g., `Pending`,
//!   `Committed`, or `Discarded`).
//!
//! ## Example
//!
//! The following example demonstrates how to create and submit a transaction:
//!
//! ```rust
//! use miden_client::Client;
//! use miden_client::auth::TransactionAuthenticator;
//! use miden_client::crypto::FeltRng;
//! use miden_client::transaction::{PaymentNoteDescription, TransactionRequestBuilder};
//! use miden_protocol::account::AccountId;
//! use miden_protocol::asset::FungibleAsset;
//! use miden_protocol::note::NoteType;
//! # use std::error::Error;
//!
//! /// Executes, proves and submits a P2ID transaction.
//! ///
//! /// This transaction is executed by `sender_id`, and creates an output note
//! /// containing 100 tokens of `faucet_id`'s fungible asset.
//! async fn create_and_submit_transaction<
//!     R: rand::Rng,
//!     AUTH: TransactionAuthenticator + Sync + 'static,
//! >(
//!     client: &mut Client<AUTH>,
//!     sender_id: AccountId,
//!     target_id: AccountId,
//!     faucet_id: AccountId,
//! ) -> Result<(), Box<dyn Error>> {
//!     // Create an asset representing the amount to be transferred.
//!     let asset = FungibleAsset::new(faucet_id, 100)?;
//!
//!     // Build a transaction request for a pay-to-id transaction.
//!     let tx_request = TransactionRequestBuilder::new().build_pay_to_id(
//!         PaymentNoteDescription::new(vec![asset.into()], sender_id, target_id),
//!         NoteType::Private,
//!         client.rng(),
//!     )?;
//!
//!     // Execute, prove, and submit the transaction in a single call.
//!     let _tx_id = client.submit_new_transaction(sender_id, tx_request).await?;
//!
//!     Ok(())
//! }
//! ```
//!
//! For more detailed information about each function and error type, refer to the specific API
//! documentation.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use miden_protocol::account::{AccountCode, AccountCodeInterface, AccountId, PartialAccount};
use miden_protocol::asset::{Asset, NonFungibleAsset};
use miden_protocol::block::{BlockHeader, BlockNumber, FeeParameters};
use miden_protocol::errors::AssetError;
use miden_protocol::note::{
    Note,
    NoteAttachments,
    NoteDetails,
    NoteId,
    NoteRecipient,
    NoteScript,
    NoteTag,
};
use miden_protocol::transaction::{AccountInputs, PartialBlockchain};
use miden_protocol::vm::MIN_STACK_DEPTH;
use miden_protocol::{Felt, Word};
use miden_standards::account::auth::FeeConversionInfo;
use miden_standards::account::faucets::FungibleFaucet;
use miden_standards::account::interface::AccountComponentInterfaceExt;
use miden_standards::note::TxFeeNote;
use miden_tx::{DataStore, NoteConsumptionChecker, TransactionExecutor};
use tracing::info;

use super::Client;
use crate::ClientError;
use crate::note::{NoteScreenerError, NoteUpdateTracker, StandardNote};
use crate::rpc::domain::account::{
    AccountStorageRequirements,
    GetAccountRequest,
    StorageMapFetch,
    VaultFetch,
};
use crate::rpc::encryption::{TransactionEncryptionKey, seal_transaction_inputs};
use crate::rpc::{AccountStateAt, NodeRpcClient, RpcError};
use crate::store::data_store::{ClientDataStore, build_partial_mmr_with_paths};
use crate::store::input_note_states::ExpectedNoteState;
use crate::store::{
    AccountRecord,
    InputNoteRecord,
    InputNoteState,
    NoteFilter,
    NoteRecordError,
    OutputNoteRecord,
    Store,
    StoreError,
    TransactionFilter,
};
use crate::sync::NoteTagRecord;
use crate::transaction::batch::InMemoryBatchDataStore;

pub mod batch;
pub use batch::{BatchBuilder, BatchBuilderError};

mod chain_anchor;
pub use chain_anchor::{ChainAnchor, ChainAnchorError};

#[cfg(feature = "dap")]
mod dap_executor;
mod prover;
pub use prover::TransactionProver;

mod record;
pub use record::{
    DiscardCause,
    TransactionDetails,
    TransactionRecord,
    TransactionStatus,
    TransactionStatusVariant,
};

mod store_update;
pub use store_update::TransactionStoreUpdate;

mod request;
pub use request::{
    ForeignAccount,
    NoteArgs,
    PaymentNoteDescription,
    PswapTransactionData,
    SwapTransactionData,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionRequestError,
    TransactionScriptTemplate,
    build_fpi_script,
};

mod observer;
pub use observer::TransactionObserver;

mod result;
// RE-EXPORTS
// ================================================================================================
pub use miden_protocol::transaction::{
    ExecutedTransaction,
    InputNote,
    InputNotes,
    OutputNote,
    OutputNotes,
    ProvenTransaction,
    PublicOutputNote,
    RawOutputNote,
    RawOutputNotes,
    TransactionArgs,
    TransactionId,
    TransactionInputs,
    TransactionKernel,
    TransactionScript,
    TransactionScriptRoot,
    TransactionSummary,
};
pub use miden_protocol::vm::{AdviceInputs, AdviceMap};
pub use miden_standards::account::interface::{AccountComponentInterface, AccountInterface};
pub use miden_standards::tx_script::{
    ExpirationTransactionScript,
    SendNotesTransactionScriptError,
};
pub use miden_tx::auth::TransactionAuthenticator;
pub use miden_tx::{
    DataStoreError,
    LocalTransactionProver,
    ProvingOptions,
    TransactionExecutorError,
    TransactionProverError,
};
pub use result::TransactionResult;

// CONSTANTS
// ================================================================================================

/// Salt the client commits native fee conversion info under when the request declares none.
///
/// See [`attach_native_fee_conversion_info`] for why this is a constant.
pub(crate) const NATIVE_FEE_CONVERSION_SALT: Word = Word::empty();

/// Transaction management methods
impl<AUTH> Client<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    // TRANSACTION DATA RETRIEVAL
    // --------------------------------------------------------------------------------------------

    /// Retrieves tracked transactions, filtered by [`TransactionFilter`].
    pub async fn get_transactions(
        &self,
        filter: TransactionFilter,
    ) -> Result<Vec<TransactionRecord>, ClientError> {
        self.store.get_transactions(filter).await.map_err(Into::into)
    }

    // TRANSACTION BATCH
    // --------------------------------------------------------------------------------------------

    /// Open a new [`BatchBuilder`] for accumulating transactions across one or more local
    /// accounts.
    ///
    /// See [`crate::transaction::batch`] for usage and constraints.
    pub fn new_transaction_batch(&mut self) -> BatchBuilder<'_, AUTH> {
        let inner_data_store = ClientDataStore::new(self.store.clone(), self.rpc_api.clone());
        BatchBuilder {
            client: self,
            data_store: InMemoryBatchDataStore::new(inner_data_store),
            pushed_txs: Vec::new(),
            consumed_input_notes: BTreeSet::new(),
        }
    }

    // TRANSACTION
    // --------------------------------------------------------------------------------------------

    /// Executes a transaction specified by the request against the specified account,
    /// proves it, submits it to the network, and updates the local database.
    ///
    /// Uses the client's default prover (configured via
    /// [`crate::builder::ClientBuilder::prover`]).
    pub async fn submit_new_transaction(
        &mut self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
    ) -> Result<TransactionId, ClientError> {
        let prover = self.tx_prover.clone();
        self.submit_new_transaction_with_prover(account_id, transaction_request, prover)
            .await
    }

    /// Executes a transaction specified by the request against the specified account,
    /// proves it with the provided prover, submits it to the network, and updates the local
    /// database.
    ///
    /// This is useful for falling back to a different prover (e.g., local) when the default
    /// prover (e.g., remote) fails with a [`ClientError::TransactionProvingError`].
    pub async fn submit_new_transaction_with_prover(
        &mut self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
        tx_prover: Arc<dyn TransactionProver>,
    ) -> Result<TransactionId, ClientError> {
        // Register any missing NTX scripts before the main transaction.
        // The registration path contains its own full execute -> prove -> submit pipeline.
        if !transaction_request.expected_ntx_scripts().is_empty() {
            Box::pin(self.ensure_ntx_scripts_registered(
                account_id,
                transaction_request.expected_ntx_scripts(),
                tx_prover.clone(),
            ))
            .await?;
        }

        let tx_result = self.execute_transaction(account_id, transaction_request).await?;
        let tx_id = tx_result.executed_transaction().id();

        let proven_transaction = self.prove_transaction_with(&tx_result, tx_prover).await?;
        let submission_height =
            self.submit_proven_transaction(proven_transaction, &tx_result).await?;

        // The transaction has been accepted by the node; the local store update
        // is a separate step that can fail independently. On failure, return a
        // distinct error carrying the pending update so the caller can decide
        // how to recover (re-apply later via `apply_transaction_update`,
        // persist for the next session, etc.).
        //
        // The update is boxed so it does not inflate the enclosing future
        // across await points (triggers clippy::large_futures).
        let tx_update =
            Box::new(self.get_transaction_store_update(&tx_result, submission_height).await?);

        if let Err(apply_err) = self.apply_transaction_update((*tx_update).clone()).await {
            info!(
                "apply_transaction_update failed for submitted tx {tx_id}; returning \
                 ApplyTransactionAfterSubmitFailed with the pending update attached: {apply_err}"
            );
            return Err(ClientError::ApplyTransactionAfterSubmitFailed {
                pending_update: tx_update,
                source: Box::new(apply_err),
            });
        }

        // Fire transaction observers (mirrors `apply_transaction`). Per-observer failures are
        // logged and never propagate — they're feature-specific side-channels, not part of the
        // submit contract.
        for observer in &self.transaction_observers {
            crate::errors::log_observer_failure(
                observer.name(),
                "TransactionObserver::apply",
                observer.apply(&tx_result).await,
            );
        }

        Ok(tx_id)
    }

    /// Creates and executes a transaction specified by the request against the specified account,
    /// but doesn't change the local database.
    ///
    /// # Errors
    ///
    /// - Returns [`ClientError::MissingOutputRecipients`] if the [`TransactionRequest`] output
    ///   notes are not a subset of executor's output notes.
    /// - Returns a [`ClientError::TransactionExecutorError`] if the execution fails.
    /// - Returns a [`ClientError::TransactionRequestError`] if the request is invalid.
    pub async fn execute_transaction(
        &self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
    ) -> Result<TransactionResult, ClientError> {
        self.execute_transaction_with_mode(
            account_id,
            transaction_request,
            TransactionExecutionMode::Standard,
            None,
        )
        .await
    }

    /// Creates and executes a transaction specified by the request against the specified account,
    /// using the provided [`ChainAnchor`] as the reference block instead of the current sync
    /// height. Like [`Self::execute_transaction`], it doesn't change the local database.
    ///
    /// Since protocol 0.16 the signed transaction summary binds the reference block commitment,
    /// so signatures collected over a summary only authorize an execution whose reference block
    /// is the one the summary was built at. This method makes such an execution reproducible on
    /// any client, regardless of its sync height: the anchor supplies the reference block header
    /// and a consistent [`PartialBlockchain`], typically captured by the transaction's original
    /// proposer via [`Self::chain_anchor_for_request`] and shipped alongside the signed data.
    ///
    /// The anchor pins the reference block only. The mode each input note is consumed in also
    /// enters the summary, so a request shared across clients should pin it through
    /// [`TransactionRequestBuilder::explicit_input_notes`]. Otherwise each client classifies the
    /// notes from its own store, and two clients can commit to different input notes.
    ///
    /// Callers holding an anchor from an untrusted source should first compare
    /// [`ChainAnchor::block_commitment`] against an independently trusted value (e.g. the block
    /// commitment bound into the signed transaction summary).
    ///
    /// Foreign account proofs are fetched at the anchor's block, so requests with foreign
    /// accounts additionally require the node to serve account state at that block.
    ///
    /// # Errors
    ///
    /// In addition to the [`Self::execute_transaction`] errors:
    /// - Returns [`ClientError::ChainAnchorError`] if an authenticated input note's creation block
    ///   is not tracked by the anchor.
    /// - Returns a [`ClientError::TransactionExecutorError`] if an input note was created after the
    ///   anchored reference block.
    /// - Returns [`ChainAnchorError::AnchoredTransactionExpired`] if the executed transaction's
    ///   expiration block has already been reached, which the network would reject.
    pub async fn execute_transaction_at(
        &mut self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
        anchor: ChainAnchor,
    ) -> Result<TransactionResult, ClientError> {
        let result = self
            .execute_transaction_with_mode(
                account_id,
                transaction_request,
                TransactionExecutionMode::Standard,
                Some(Box::new(anchor)),
            )
            .await?;

        // The expiration delta counts from the anchored reference block, so a stale anchor can
        // yield an already-expired transaction, which the network would only reject after the
        // caller has paid for proving. The sync height never runs ahead of the real tip, so this
        // fires only on transactions that are certainly too late.
        let expiration = result.executed_transaction().expiration_block_num();
        let sync_height = self.store.get_sync_height().await?;
        if expiration <= sync_height {
            return Err(
                ChainAnchorError::AnchoredTransactionExpired { expiration, sync_height }.into()
            );
        }

        Ok(result)
    }

    /// Captures a [`ChainAnchor`] at the client's current sync height, tracking the blocks in
    /// `tracked_blocks` (in addition to the reference block itself, which needs no tracking) so
    /// that transactions consuming authenticated notes created in those blocks can later execute
    /// against the anchor.
    async fn chain_anchor_at_tip(
        &self,
        tracked_blocks: BTreeSet<BlockNumber>,
    ) -> Result<ChainAnchor, ClientError> {
        let sync_height = self.store.get_sync_height().await?;

        let (header, _had_notes) = self
            .store
            .get_block_header_by_num(sync_height)
            .await?
            .ok_or(StoreError::BlockHeaderNotFound(sync_height))?;

        let mut tracked_blocks = tracked_blocks;
        // The kernel extends the MMR with the reference block itself, so it needs no path.
        tracked_blocks.remove(&sync_height);

        let block_headers: Vec<BlockHeader> = self
            .store
            .get_block_headers(&tracked_blocks)
            .await?
            .into_iter()
            .map(|(header, _has_notes)| header)
            .collect();

        // `Store::get_block_headers` may silently omit missing headers, so verify each requested
        // block is present rather than comparing lengths.
        let fetched_nums: BTreeSet<BlockNumber> =
            block_headers.iter().map(BlockHeader::block_num).collect();
        if let Some(&missing) = tracked_blocks.difference(&fetched_nums).next() {
            return Err(StoreError::BlockHeaderNotFound(missing).into());
        }

        let peaks = self.store.get_current_blockchain_peaks().await?;
        let partial_mmr = build_partial_mmr_with_paths(&self.store, peaks, &block_headers).await?;

        let chain = PartialBlockchain::new(partial_mmr, block_headers)?;

        Ok(ChainAnchor::new(header, chain)?)
    }

    /// Captures a [`ChainAnchor`] at the client's current sync height, tracking the creation
    /// blocks of the request's authenticated input notes so that the request can later execute
    /// against the anchor. A note pinned through
    /// [`TransactionRequestBuilder::explicit_input_notes`] contributes the block of the proof
    /// it carries. The store's inclusion proof decides for every other note.
    ///
    /// This is the capture entry point for flows that never see a successful execution result at
    /// capture time — e.g. multisig proposal flows, where execution intentionally fails with
    /// [`TransactionExecutorError::Unauthorized`] to surface the transaction summary for signing.
    /// Capture the anchor first, execute the request with [`Self::execute_transaction_at`], and
    /// ship the anchor alongside the summary; the same anchor then reproduces the summary during
    /// later verification and execution.
    ///
    /// # Errors
    ///
    /// - Returns [`ClientError::StoreError`] if a header for the sync height or a tracked block is
    ///   not present in the store.
    /// - Returns [`ChainAnchorError::TooManyTrackedBlocks`] if the request's authenticated input
    ///   notes were created across more blocks than a transaction can reference.
    pub async fn chain_anchor_for_request(
        &self,
        transaction_request: &TransactionRequest,
    ) -> Result<ChainAnchor, ClientError> {
        let mut tracked_blocks: BTreeSet<BlockNumber> = transaction_request
            .explicit_input_note_proofs()
            .map(|proof| proof.location().block_num())
            .collect();

        let inferred_input_note_ids: Vec<NoteId> =
            transaction_request.inferred_input_note_ids().collect();
        if !inferred_input_note_ids.is_empty() {
            tracked_blocks.extend(
                self.store
                    .get_input_notes(NoteFilter::List(inferred_input_note_ids))
                    .await?
                    .iter()
                    .filter(|record| record.is_authenticated())
                    .filter_map(|record| record.inclusion_proof())
                    .map(|proof| proof.location().block_num()),
            );
        }

        self.chain_anchor_at_tip(tracked_blocks).await
    }

    /// Executes `transaction_request` (e.g. consuming a note) through the DAP program executor,
    /// so a DAP client can attach and step through the whole transaction — kernel, note scripts,
    /// and account code — instead of only a standalone transaction script.
    ///
    /// This is a debugging entry point: it runs the transaction interactively under the debug
    /// adapter and does not prove, submit, or apply the result. The listen address (and optional
    /// replay-snapshot path) are taken from the globally installed
    /// [`DapConfig`](miden_debug::DapConfig).
    ///
    /// # Errors
    ///
    /// This applies the same request preparation and output-recipient validation as
    /// [`Self::execute_transaction`], and returns the corresponding [`ClientError`] on failure.
    #[cfg(feature = "dap")]
    pub async fn execute_transaction_with_dap(
        &self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
    ) -> Result<TransactionResult, ClientError> {
        self.execute_transaction_with_mode(
            account_id,
            transaction_request,
            TransactionExecutionMode::Dap,
            None,
        )
        .await
    }

    /// Executes a prepared transaction with the selected program executor while keeping request
    /// preparation, data-store population, note filtering, and result validation identical across
    /// execution modes.
    async fn execute_transaction_with_mode(
        &self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
        execution_mode: TransactionExecutionMode,
        anchor: Option<Box<ChainAnchor>>,
    ) -> Result<TransactionResult, ClientError> {
        let account: PartialAccount =
            self.get_native_account_record(account_id).await?.try_into()?;

        let prep = self
            .prepare_transaction(&account, transaction_request, anchor.as_deref())
            .await?;

        let mut data_store = ClientDataStore::new(self.store.clone(), self.rpc_api.clone());
        if let Some(anchor) = anchor {
            data_store = data_store.with_chain_anchor(*anchor);
        }
        data_store.register_note_scripts(prep.output_note_scripts());
        for fpi_account in &prep.foreign_account_inputs {
            data_store.mast_store().load_account_code(fpi_account.code());
        }
        data_store.register_foreign_account_inputs(prep.foreign_account_inputs);

        data_store.mast_store().load_account_code(account.code());

        let mut notes = prep.notes;
        if prep.ignore_invalid_notes {
            notes = self
                .get_valid_input_notes(
                    &data_store,
                    account.id(),
                    prep.block_num,
                    notes,
                    prep.tx_args.clone(),
                )
                .await?;
        }

        let executed_transaction = match execution_mode {
            TransactionExecutionMode::Standard => {
                self.build_executor(&data_store)?
                    .execute_transaction(account_id, prep.block_num, notes, prep.tx_args)
                    .await?
            },
            #[cfg(feature = "dap")]
            TransactionExecutionMode::Dap => {
                self.build_dap_executor(&data_store)?
                    .execute_transaction(account_id, prep.block_num, notes, prep.tx_args)
                    .await?
            },
        };

        validate_executed_transaction(&executed_transaction, &prep.output_recipients)?;
        TransactionResult::new(executed_transaction, prep.future_notes)
    }

    /// Performs the data-store-independent setup shared by `execute_transaction` and
    /// `execute_transaction_for_batch`: validates the request against the account's committed
    /// store state, loads/filters input notes, builds the transaction script and args, retrieves
    /// foreign-account inputs, and computes the reference block number.
    ///
    /// This method does not write to the store: any state produced by the transaction is
    /// persisted only after the transaction executes successfully.
    ///
    /// In batch execution, request validation is skipped: the committed store state does not
    /// reflect balances stacked by prior in-batch pushes, so validating against it would wrongly
    /// reject transactions the executor accepts.
    ///
    /// When `anchor` is provided, the reference block is the anchor's block instead of the
    /// current sync height, and the recency check is skipped — anchored execution deliberately
    /// references a block older than the tip.
    pub(crate) async fn prepare_transaction(
        &self,
        account: &PartialAccount,
        transaction_request: TransactionRequest,
        anchor: Option<&ChainAnchor>,
    ) -> Result<PreparedTransaction, ClientError> {
        self.validate_account_request(
            &transaction_request,
            account.id(),
            &account.code_interface(),
        )
        .await?;

        self.prepare_transaction_inner(account.code_interface(), transaction_request, anchor)
            .await
    }

    pub(crate) async fn prepare_transaction_for_batch(
        &self,
        account: &PartialAccount,
        transaction_request: TransactionRequest,
    ) -> Result<PreparedTransaction, ClientError> {
        self.prepare_transaction_inner(account.code_interface(), transaction_request, None)
            .await
    }

    async fn prepare_transaction_inner(
        &self,
        account_code_interface: AccountCodeInterface,
        mut transaction_request: TransactionRequest,
        anchor: Option<&ChainAnchor>,
    ) -> Result<PreparedTransaction, ClientError> {
        if anchor.is_none() {
            self.validate_recency().await?;
        }

        // Retrieve all input notes from the store.
        let mut stored_note_records = self
            .store
            .get_input_notes(NoteFilter::List(transaction_request.input_note_ids().collect()))
            .await?;

        // Verify that none of the authenticated input notes are already consumed.
        for note in &stored_note_records {
            if note.is_consumed() {
                let id = note.id().expect(
                    "stored note records reaching this check carry metadata so id() is Some",
                );
                return Err(ClientError::TransactionRequestError(
                    TransactionRequestError::InputNoteAlreadyConsumed(id),
                ));
            }
        }

        // Only keep authenticated input notes from the store.
        stored_note_records.retain(InputNoteRecord::is_authenticated);

        let notes = transaction_request.build_input_notes(stored_note_records)?;

        // Each authenticated note's creation block must be tracked by the anchor; fail with a
        // typed error so callers can recapture a wider anchor. Notes newer than the anchor are
        // left for the executor to reject.
        if let Some(anchor) = anchor {
            for note in notes.iter() {
                if let Some(location) = note.location() {
                    let block_num = location.block_num();
                    if block_num < anchor.block_num()
                        && !anchor.partial_blockchain().contains_block(block_num)
                    {
                        return Err(ChainAnchorError::BlockNotTracked { block_num }.into());
                    }
                }
            }
        }

        let output_recipients =
            transaction_request.expected_output_recipients().cloned().collect::<Vec<_>>();

        let future_notes: Vec<(NoteDetails, NoteTag)> =
            transaction_request.expected_future_notes().cloned().collect();

        let tx_script = transaction_request.build_transaction_script(&account_code_interface)?;

        let foreign_accounts = transaction_request.foreign_accounts().clone();

        // The reference block: the anchor's block when pinned, the sync height otherwise.
        // Foreign account proofs are fetched at this block to stay consistent with it.
        let block_num = match anchor {
            Some(anchor) => anchor.block_num(),
            None => self.store.get_sync_height().await?,
        };

        let foreign_account_inputs =
            self.retrieve_foreign_account_inputs(foreign_accounts, block_num).await?;

        let ignore_invalid_notes = transaction_request.ignore_invalid_input_notes();

        let reference_header = match anchor {
            Some(anchor) => anchor.header().clone(),
            None => {
                self.store
                    .get_block_header_by_num(block_num)
                    .await?
                    .ok_or(StoreError::BlockHeaderNotFound(block_num))?
                    .0
            },
        };
        attach_native_fee_conversion_info(
            &mut transaction_request,
            &account_code_interface,
            &reference_header,
        )?;

        let tx_args = transaction_request.into_transaction_args(tx_script);

        Ok(PreparedTransaction {
            notes,
            output_recipients,
            future_notes,
            tx_args,
            foreign_account_inputs,
            block_num,
            ignore_invalid_notes,
        })
    }

    /// Proves the specified transaction using the prover configured for this client.
    pub async fn prove_transaction(
        &self,
        tx_result: &TransactionResult,
    ) -> Result<ProvenTransaction, ClientError> {
        self.prove_transaction_with(tx_result, self.tx_prover.clone()).await
    }

    /// Proves the specified transaction using the provided prover.
    ///
    /// # Errors
    ///
    /// - Returns a [`ClientError::TransactionProvingError`] if the prover fails to produce a proof.
    /// - Returns a [`ClientError::MismatchedProvenTransaction`] if the prover returns a proof of a
    ///   transaction other than the requested one.
    pub async fn prove_transaction_with(
        &self,
        tx_result: &TransactionResult,
        tx_prover: Arc<dyn TransactionProver>,
    ) -> Result<ProvenTransaction, ClientError> {
        info!("Proving transaction...");

        let executed_transaction = tx_result.executed_transaction();
        let proven_transaction = tx_prover.prove(executed_transaction.clone().into()).await?;

        // A prover is trusted with the witness, but not with choosing which transaction gets
        // submitted. Everything downstream (submission, the local store update, the returned
        // id) is derived from `tx_result`, so a proof of anything else would be submitted
        // while the local state recorded the transaction that never reached the network.
        //
        // The id commits to the initial and final account commitments and to the input and
        // output note commitments; the account commitments in turn commit to the account id,
        // so a matching id covers the account as well.
        if proven_transaction.id() != executed_transaction.id() {
            return Err(ClientError::MismatchedProvenTransaction {
                requested: executed_transaction.id(),
                returned: proven_transaction.id(),
            });
        }

        info!("Transaction proven.");

        Ok(proven_transaction)
    }

    /// Submits a previously proven transaction to the RPC endpoint and returns the node’s chain tip
    /// upon mempool admission.
    pub async fn submit_proven_transaction(
        &mut self,
        proven_transaction: ProvenTransaction,
        transaction_inputs: impl Into<TransactionInputs>,
    ) -> Result<BlockNumber, ClientError> {
        info!("Submitting transaction to the network...");
        let tx_id = proven_transaction.id();
        let key = self.transaction_encryption_key().await?;
        let sealed_inputs =
            seal_transaction_inputs(&mut self.rng, &key, tx_id, &transaction_inputs.into())?;
        let result =
            self.rpc_api.submit_proven_transaction(proven_transaction, sealed_inputs).await;
        if let Err(err) = &result {
            self.forget_stale_transaction_encryption_key(err).await;
        }
        let block_num = result?;
        info!("Transaction submitted.");

        Ok(block_num)
    }

    /// Returns the validator set's transaction encryption key, fetching and verifying it on first
    /// use.
    ///
    /// The key is public data shared by the whole validator set, so it is cached in the store and
    /// reused across submissions and restarts. A freshly fetched key is verified against the
    /// validator set committed in the chain tip before it is cached or used: the endpoint is served
    /// by the RPC operator, which is the party the encryption keeps out.
    pub(crate) async fn transaction_encryption_key(
        &self,
    ) -> Result<TransactionEncryptionKey, ClientError> {
        if let Some(key) = self.store.get_transaction_encryption_key().await? {
            return Ok(key);
        }

        let attested = self.rpc_api.get_transaction_encryption_key().await?;

        // The genesis commitment scopes the attestation to this chain, and the chain tip carries
        // the validator set currently entitled to attest. Both come from the local store, so a
        // response cannot supply its own trust anchor.
        let genesis_commitment =
            self.trusted_block_header(BlockNumber::GENESIS).await?.commitment();
        let chain_tip = self.store.get_sync_height().await?;
        let validator_keys = self.trusted_block_header(chain_tip).await?.validator_keys().clone();

        let key = attested.verify(genesis_commitment, &validator_keys)?;
        self.store.set_transaction_encryption_key(&key).await?;

        Ok(key)
    }

    /// Installs the transaction encryption key that submission seals against, skipping the fetch
    /// and its attestation check.
    #[cfg(feature = "testing")]
    pub async fn seed_transaction_encryption_key(
        &self,
        key: TransactionEncryptionKey,
    ) -> Result<(), ClientError> {
        Ok(self.store.set_transaction_encryption_key(&key).await?)
    }

    /// Evicts the cached encryption key when a submission was rejected for having been sealed
    /// against a key the validator does not hold, so the next submission fetches a fresh one.
    ///
    /// An eviction failure is logged rather than returned: the caller is already reporting the
    /// submission error, which the store error must not mask.
    pub(crate) async fn forget_stale_transaction_encryption_key(&self, err: &RpcError) {
        if err.is_stale_transaction_encryption_key()
            && let Err(err) = self.store.remove_transaction_encryption_key().await
        {
            tracing::warn!("failed to evict the stale transaction encryption key: {err}");
        }
    }

    /// Returns a locally stored block header, which the client has already authenticated during
    /// sync.
    ///
    /// # Errors
    /// Returns an error if the header is not stored locally, which means the client has not synced
    /// far enough to have a trust anchor.
    async fn trusted_block_header(
        &self,
        block_num: BlockNumber,
    ) -> Result<BlockHeader, ClientError> {
        self.store.get_block_header_by_num(block_num).await?.map(|(header, _)| header).ok_or_else(
            || {
                ClientError::ChainValidationError(alloc::format!(
                    "block header {block_num} is not tracked locally; sync the client before it can verify data against the chain"
                ))
            },
        )
    }

    /// Builds a [`TransactionStoreUpdate`] for the provided transaction result at the specified
    /// submission height.
    pub async fn get_transaction_store_update(
        &self,
        tx_result: &TransactionResult,
        submission_height: BlockNumber,
    ) -> Result<TransactionStoreUpdate, TransactionStoreUpdateError> {
        let note_updates = self.get_note_updates(submission_height, tx_result).await?;

        // Only expected input notes need tags; output notes are committed (with proofs)
        // via account-matched transaction sync.
        let new_tags: Vec<NoteTagRecord> = note_updates
            .updated_input_notes()
            .filter_map(|note| {
                let note = note.inner();

                if let InputNoteState::Expected(ExpectedNoteState { tag: Some(tag), .. }) =
                    note.state()
                {
                    Some(NoteTagRecord::with_note_source(*tag, note.details_commitment()))
                } else {
                    None
                }
            })
            .collect();

        Ok(TransactionStoreUpdate::new(
            tx_result.executed_transaction().clone(),
            submission_height,
            note_updates,
            tx_result.future_notes().to_vec(),
            new_tags,
        ))
    }

    /// Persists the effects of a submitted transaction into the local store,
    /// updating account data, note metadata, and future note tracking.
    pub async fn apply_transaction(
        &self,
        tx_result: &TransactionResult,
        submission_height: BlockNumber,
    ) -> Result<(), ClientError> {
        let tx_update = self.get_transaction_store_update(tx_result, submission_height).await?;

        self.apply_transaction_update(tx_update).await?;

        // Fire transaction observers. Per-observer failures are logged.
        for observer in &self.transaction_observers {
            if let Err(err) = observer.apply(tx_result).await {
                tracing::warn!(
                    observer = observer.name(),
                    error = ?err,
                    "TransactionObserver::apply failed; continuing with remaining observers",
                );
            }
        }

        Ok(())
    }

    pub async fn apply_transaction_update(
        &self,
        tx_update: TransactionStoreUpdate,
    ) -> Result<(), ClientError> {
        // Transaction was proven and submitted to the node correctly, persist note details and
        // update account
        info!("Applying transaction to the local store...");

        let executed_transaction = tx_update.executed_transaction();
        let account_id = executed_transaction.account_id();

        if self.account_reader(account_id).status().await?.is_locked() {
            return Err(ClientError::AccountLocked(account_id));
        }

        self.store.apply_transaction(tx_update).await?;
        info!("Transaction stored.");
        Ok(())
    }

    /// Executes the provided transaction script against the specified account, and returns the
    /// resulting stack. Advice inputs and foreign accounts can be provided for the execution.
    ///
    /// The transaction will use the current sync height as the block reference.
    pub async fn execute_program(
        &self,
        account_id: AccountId,
        tx_script: TransactionScript,
        advice_inputs: AdviceInputs,
        foreign_accounts: BTreeMap<AccountId, ForeignAccount>,
    ) -> Result<[Felt; MIN_STACK_DEPTH], ClientError> {
        let (data_store, block_ref) =
            self.prepare_program_execution(account_id, foreign_accounts).await?;

        Ok(self
            .build_executor(&data_store)?
            .execute_tx_view_script(account_id, block_ref, tx_script, advice_inputs)
            .await?)
    }

    /// Executes the provided transaction script with a DAP debug adapter listening for
    /// connections, allowing interactive debugging via any DAP-compatible client.
    #[cfg(feature = "dap")]
    pub async fn execute_program_with_dap(
        &self,
        account_id: AccountId,
        tx_script: TransactionScript,
        advice_inputs: AdviceInputs,
        foreign_accounts: BTreeMap<AccountId, ForeignAccount>,
    ) -> Result<[Felt; MIN_STACK_DEPTH], ClientError> {
        let (data_store, block_ref) =
            self.prepare_program_execution(account_id, foreign_accounts).await?;

        Ok(self
            .build_dap_executor(&data_store)?
            .execute_tx_view_script(account_id, block_ref, tx_script, advice_inputs)
            .await?)
    }

    // HELPERS
    // --------------------------------------------------------------------------------------------

    /// Validates that the specified transaction request can be executed by the specified account.
    ///
    /// This does't guarantee that the transaction will succeed, but it's useful to avoid submitting
    /// transactions that are guaranteed to fail. Some of the validations include:
    /// - That the account has enough balance to cover the outgoing assets.
    /// - That the client is not too far behind the chain tip.
    pub async fn validate_request(
        &self,
        account_id: AccountId,
        transaction_request: &TransactionRequest,
    ) -> Result<(), ClientError> {
        self.validate_recency().await?;
        validate_output_note_senders(transaction_request, account_id)?;
        let account: PartialAccount = self
            .store
            .get_minimal_partial_account(account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(account_id))?
            .try_into()?;
        self.validate_account_request(transaction_request, account_id, &account.code_interface())
            .await
    }

    /// Validates the request against the account's committed store state: faucet accounts are
    /// accepted as-is, other accounts get their vault asset list checked against the request's
    /// outgoing assets. Only the asset list is loaded from the store; the account itself is not
    /// reconstructed.
    async fn validate_account_request(
        &self,
        transaction_request: &TransactionRequest,
        account_id: AccountId,
        account_code_interface: &AccountCodeInterface,
    ) -> Result<(), ClientError> {
        validate_fee_conversion_info_support(transaction_request, account_code_interface)?;

        if account_code_interface.contains([FungibleFaucet::mint_and_send_root()]) {
            // TODO(#1266): Add faucet validations.
            Ok(())
        } else {
            let assets = self.account_reader(account_id).assets().await?;
            validate_basic_account_request(transaction_request, &assets)
        }
    }

    async fn validate_recency(&self) -> Result<(), ClientError> {
        if let Some(max_block_number_delta) = self.max_block_number_delta {
            let current_chain_tip =
                self.rpc_api.get_block_header_by_number(None, false).await?.0.block_num();

            if current_chain_tip > self.store.get_sync_height().await? + max_block_number_delta {
                return Err(ClientError::RecencyConditionError(
                    "The client is too far behind the chain tip to execute the transaction",
                ));
            }
        }
        Ok(())
    }

    /// Checks whether the node's `note_scripts` registry already has each of the expected NTX
    /// scripts. For any script that is missing, creates and submits a registration transaction
    /// that produces a public note carrying that script.
    ///
    /// `account_id` is the account that will execute the registration transaction.
    ///
    /// Standard note scripts are skipped — the NTX builder resolves those directly, so they
    /// never need registering. A missing non-standard script is registered, not an error.
    ///
    /// This method is called automatically by [`Self::submit_new_transaction_with_prover`] when the
    /// [`TransactionRequest`] contains expected NTX scripts. It can also be called directly if
    /// you want to register scripts ahead of time.
    pub async fn ensure_ntx_scripts_registered(
        &mut self,
        account_id: AccountId,
        scripts: &[NoteScript],
        tx_prover: Arc<dyn TransactionProver>,
    ) -> Result<(), ClientError> {
        let mut missing_scripts = Vec::new();

        for script in scripts {
            // Standard scripts are resolved by the NTX builder directly; no registration needed.
            if StandardNote::from_script(script).is_some() {
                continue;
            }

            let script_root = script.root();

            // Scripts the node doesn't have are queued for registration; only RPC errors abort.
            match self.rpc_api.get_note_script_by_root(script_root.into()).await {
                Ok(Some(_)) => {},
                Ok(None) => missing_scripts.push(script.clone()),
                Err(source) => {
                    return Err(ClientError::NtxScriptRegistrationFailed {
                        script_root: script_root.into(),
                        source,
                    });
                },
            }
        }

        if missing_scripts.is_empty() {
            return Ok(());
        }

        let registration_request = TransactionRequestBuilder::new().build_register_note_scripts(
            account_id,
            missing_scripts,
            self.rng(),
        )?;

        let tx_result = self.execute_transaction(account_id, registration_request).await?;
        let proven = self.prove_transaction_with(&tx_result, tx_prover).await?;
        let submission_height = self.submit_proven_transaction(proven, &tx_result).await?;
        self.apply_transaction(&tx_result, submission_height).await?;

        Ok(())
    }

    /// Filters the provided input notes down to the subset that can be consumed by the account.
    ///
    /// The provided data store must already have the account's code loaded and the request's
    /// output note scripts registered, so output note creation can resolve them without them
    /// being present in the store.
    ///
    /// The trial runs against `data_store` at `block_ref`, which must match the reference block
    /// the actual execution will use.
    pub(crate) async fn get_valid_input_notes<STORE: DataStore + Sync>(
        &self,
        data_store: &STORE,
        account_id: AccountId,
        block_ref: BlockNumber,
        mut input_notes: InputNotes<InputNote>,
        tx_args: TransactionArgs,
    ) -> Result<InputNotes<InputNote>, ClientError> {
        loop {
            // The consumption checker rejects a zero-note call; the set can be empty because the
            // request carried no notes or because screening removed them all.
            if input_notes.is_empty() {
                break;
            }

            let execution = NoteConsumptionChecker::new(&self.build_executor(data_store)?)
                .check_notes_consumability(
                    account_id,
                    block_ref,
                    input_notes.iter().map(|n| n.clone().into_note()).collect(),
                    tx_args.clone(),
                )
                .await?;

            if execution.failed().is_empty() {
                break;
            }

            let failed_note_ids: BTreeSet<NoteId> =
                execution.failed().iter().map(|n| n.note().id()).collect();
            let filtered_input_notes = InputNotes::new(
                input_notes
                    .into_iter()
                    .filter(|note| !failed_note_ids.contains(&note.id()))
                    .collect(),
            )
            .expect("Created from a valid input notes list");

            input_notes = filtered_input_notes;
        }

        Ok(input_notes)
    }

    /// Returns foreign account inputs for the required foreign accounts specified by the
    /// transaction request, with proofs anchored at `block_num` — the transaction's reference
    /// block, so that the fetched state is consistent with the block the transaction executes
    /// against.
    ///
    /// For any [`ForeignAccount::Public`] in `foreign_accounts`, these pieces of data are retrieved
    /// from the network. For any [`ForeignAccount::Private`] account, inner data is used and only
    /// a proof of the account's existence on the network is fetched.
    async fn retrieve_foreign_account_inputs(
        &self,
        foreign_accounts: BTreeMap<AccountId, ForeignAccount>,
        block_num: BlockNumber,
    ) -> Result<Vec<AccountInputs>, ClientError> {
        if foreign_accounts.is_empty() {
            return Ok(Vec::new());
        }

        let mut return_foreign_account_inputs = Vec::with_capacity(foreign_accounts.len());

        for foreign_account in foreign_accounts.into_values() {
            let foreign_account_inputs = match foreign_account {
                ForeignAccount::Public(account_id, storage_requirements) => {
                    fetch_public_account_inputs(
                        &self.store,
                        &self.rpc_api,
                        account_id,
                        storage_requirements,
                        AccountStateAt::Block(block_num),
                    )
                    .await?
                },
                ForeignAccount::Private(partial_account) => {
                    let account_id = partial_account.id();
                    let (_, account_proof) = self
                        .rpc_api
                        .get_account(
                            account_id,
                            GetAccountRequest::new().at(AccountStateAt::Block(block_num)),
                        )
                        .await?;
                    let (witness, _) = account_proof.into_parts();
                    AccountInputs::new(partial_account, witness)
                },
            };

            return_foreign_account_inputs.push(foreign_account_inputs);
        }

        Ok(return_foreign_account_inputs)
    }

    /// Prepares the data store and block reference for program execution.
    ///
    /// This is shared setup for both `execute_program` and `execute_program_with_dap`.
    async fn prepare_program_execution(
        &self,
        account_id: AccountId,
        foreign_accounts: BTreeMap<AccountId, ForeignAccount>,
    ) -> Result<(ClientDataStore, BlockNumber), ClientError> {
        let block_ref = self.get_sync_height().await?;

        let foreign_account_inputs =
            self.retrieve_foreign_account_inputs(foreign_accounts, block_ref).await?;

        let account_code = self
            .store
            .get_account_code(account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(account_id))?;

        let data_store = ClientDataStore::new(self.store.clone(), self.rpc_api.clone());

        // Ensure code is loaded on MAST store
        data_store.mast_store().load_account_code(&account_code);

        for fpi_account in &foreign_account_inputs {
            data_store.mast_store().load_account_code(fpi_account.code());
        }

        data_store.register_foreign_account_inputs(foreign_account_inputs);

        Ok((data_store, block_ref))
    }

    /// Creates a transaction executor configured with the client's runtime options,
    /// authenticator, and source manager.
    pub(crate) fn build_executor<'store, 'auth, STORE: DataStore + Sync>(
        &'auth self,
        data_store: &'store STORE,
    ) -> Result<TransactionExecutor<'store, 'auth, STORE, AUTH>, TransactionExecutorError> {
        let mut executor = TransactionExecutor::new(data_store)
            .with_options(self.exec_options)?
            .with_source_manager(self.source_manager.clone());
        if let Some(authenticator) = self.authenticator.as_deref() {
            executor = executor.with_authenticator(authenticator);
        }
        Ok(executor)
    }

    /// Loads a minimal partial [`AccountRecord`] for an account that must be usable as a
    /// transaction's native account. Errors out if the account is not tracked or if it is
    /// watched. The full account state is never loaded: the executor reads it lazily through the
    /// [`DataStore`].
    async fn get_native_account_record(
        &self,
        account_id: AccountId,
    ) -> Result<AccountRecord, ClientError> {
        let account_record = self
            .store
            .get_minimal_partial_account(account_id)
            .await?
            .ok_or(ClientError::AccountDataNotFound(account_id))?;
        if account_record.is_watched() {
            return Err(ClientError::AccountIsWatched(account_id));
        }
        Ok(account_record)
    }

    /// Creates a transaction executor configured for DAP (Debug Adapter Protocol) debugging.
    #[cfg(feature = "dap")]
    pub(crate) fn build_dap_executor<'store, 'auth, STORE: DataStore + Sync>(
        &'auth self,
        data_store: &'store STORE,
    ) -> Result<
        TransactionExecutor<'store, 'auth, STORE, AUTH, dap_executor::DapProgramExecutor>,
        TransactionExecutorError,
    > {
        Ok(self
            .build_executor(data_store)?
            .with_program_executor::<dap_executor::DapProgramExecutor>())
    }

    /// Returns [`NoteUpdateTracker`] containing the note updates generated by an executed
    /// transaction.
    async fn get_note_updates(
        &self,
        submission_height: BlockNumber,
        tx_result: &TransactionResult,
    ) -> Result<NoteUpdateTracker, TransactionStoreUpdateError> {
        let executed_tx = tx_result.executed_transaction();
        let current_timestamp = self.store.get_current_timestamp();
        let current_block_num = self.store.get_sync_height().await?;

        // New output notes
        //
        // The kernel's fee note is excluded. It is a bearer note for whoever builds the batch, so
        // tracking it would return it from `get_output_notes(NoteFilter::All)` as a note the user
        // created, list it in `miden-client notes`, and -- because `STATE_EXPECTED_FULL` is inside
        // the `Unspent` filter -- feed its nullifier prefix into `sync_nullifiers` on every sync,
        // making the client ask the node about a note it does not own once per fee-paying
        // transaction. Nothing is lost by excluding it: the complete raw output list is already
        // kept verbatim on the transaction record (`TransactionDetails.output_notes`).
        //
        // Same discriminator, same reason as the input-note loop below.
        let new_output_notes = executed_tx
            .output_notes()
            .iter()
            .filter(|output_note| {
                output_note
                    .recipient()
                    .is_none_or(|recipient| recipient.script().root() != TxFeeNote::script_root())
            })
            .cloned()
            .filter_map(|output_note| {
                OutputNoteRecord::try_from_output_note(output_note, submission_height).ok()
            })
            .collect::<Vec<_>>();

        // New relevant input notes
        let mut new_input_notes = vec![];
        let output_notes: Vec<Note> =
            notes_from_output(executed_tx.output_notes()).cloned().collect();
        let note_screener = self.note_screener().clone();
        let output_note_relevances = note_screener.get_batch_consumability(&output_notes).await?;

        for note in output_notes {
            // The fee note is a bearer note meant for whoever builds the batch, so the screener
            // wrongly reports it as consumable here. Tracking it would also register its tag, and
            // all TX_FEE notes share one chain-wide tag, so every later sync would pull in every
            // fee note the chain has produced.
            if note.script().root() == TxFeeNote::script_root() {
                continue;
            }

            if output_note_relevances.contains_key(&note.id()) {
                let metadata = *note.metadata();
                let tag = metadata.tag();
                let attachments = note.attachments().clone();

                new_input_notes.push(InputNoteRecord::new(
                    note.into(),
                    attachments,
                    current_timestamp,
                    ExpectedNoteState {
                        metadata: Some(metadata),
                        after_block_num: submission_height,
                        tag: Some(tag),
                    }
                    .into(),
                ));
            }
        }

        // Track future input notes described in the transaction result.
        new_input_notes.extend(tx_result.future_notes().iter().map(|(note_details, tag)| {
            InputNoteRecord::new(
                note_details.clone(),
                NoteAttachments::empty(),
                None,
                ExpectedNoteState {
                    metadata: None,
                    after_block_num: current_block_num,
                    tag: Some(*tag),
                }
                .into(),
            )
        }));

        // Locally consumed notes. Notes already tracked by the store only need their state
        // advanced; the rest (the request's unauthenticated notes, which are not persisted
        // before the transaction succeeds) are tracked from this point on, so records for them
        // are built from the executed transaction's inputs.
        let consumed_note_ids =
            executed_tx.tx_inputs().input_notes().iter().map(InputNote::id).collect();

        let consumed_notes =
            self.store.get_input_notes(NoteFilter::List(consumed_note_ids)).await?;

        let tracked_note_ids =
            consumed_notes.iter().filter_map(InputNoteRecord::id).collect::<BTreeSet<_>>();

        for input_note in executed_tx.tx_inputs().input_notes() {
            if !tracked_note_ids.contains(&input_note.id()) {
                let mut input_note_record = InputNoteRecord::from(input_note.clone());
                input_note_record.consumed_locally(
                    executed_tx.account_id(),
                    executed_tx.id(),
                    current_timestamp,
                )?;
                new_input_notes.push(input_note_record);
            }
        }

        let mut updated_input_notes = vec![];

        for mut input_note_record in consumed_notes {
            if input_note_record.consumed_locally(
                executed_tx.account_id(),
                executed_tx.id(),
                current_timestamp,
            )? {
                updated_input_notes.push(input_note_record);
            }
        }

        Ok(NoteUpdateTracker::for_transaction_updates(
            new_input_notes,
            updated_input_notes,
            new_output_notes,
        ))
    }
}

// TRANSACTION STORE UPDATE ERROR
// ================================================================================================

/// Error returned by [`Client::get_transaction_store_update`] when building the store update
/// for a submitted transaction fails.
#[derive(Debug, thiserror::Error)]
pub enum TransactionStoreUpdateError {
    #[error("store error")]
    Store(#[from] StoreError),
    #[error("note screener error")]
    NoteScreener(#[from] NoteScreenerError),
    #[error("note record error")]
    NoteRecord(#[from] NoteRecordError),
}

// HELPERS
// ================================================================================================

#[derive(Clone, Copy, Debug)]
enum TransactionExecutionMode {
    Standard,
    #[cfg(feature = "dap")]
    Dap,
}

/// Data-store-independent state produced during transaction preparation.
pub(crate) struct PreparedTransaction {
    pub(crate) notes: InputNotes<InputNote>,
    pub(crate) output_recipients: Vec<NoteRecipient>,
    pub(crate) future_notes: Vec<(NoteDetails, NoteTag)>,
    pub(crate) tx_args: TransactionArgs,
    pub(crate) foreign_account_inputs: Vec<AccountInputs>,
    pub(crate) block_num: BlockNumber,
    pub(crate) ignore_invalid_notes: bool,
}

impl PreparedTransaction {
    /// Returns the scripts of the request's expected output notes. These must be registered on
    /// the executor's data store so output note creation can resolve them during execution.
    pub(crate) fn output_note_scripts(&self) -> impl Iterator<Item = NoteScript> + '_ {
        self.output_recipients.iter().map(|recipient| recipient.script().clone())
    }
}

/// Helper to get the account outgoing assets.
///
/// Any outgoing assets resulting from executing note scripts but not present in expected output
/// notes wouldn't be included.
fn get_outgoing_assets(
    transaction_request: &TransactionRequest,
) -> (BTreeMap<AccountId, u64>, Vec<NonFungibleAsset>) {
    // Get own notes assets
    let mut own_notes_assets = match transaction_request.script_template() {
        Some(TransactionScriptTemplate::SendNotes(notes)) => notes
            .iter()
            .map(|note| (note.id(), note.assets().clone()))
            .collect::<BTreeMap<_, _>>(),
        _ => BTreeMap::default(),
    };
    // Get transaction output notes assets
    let mut output_notes_assets = transaction_request
        .expected_output_own_notes()
        .into_iter()
        .map(|note| (note.id(), note.assets().clone()))
        .collect::<BTreeMap<_, _>>();

    // Merge with own notes assets and delete duplicates
    output_notes_assets.append(&mut own_notes_assets);

    // Create a map of the fungible and non-fungible assets in the output notes
    let outgoing_assets = output_notes_assets.values().flat_map(|note_assets| note_assets.iter());

    request::collect_assets(outgoing_assets)
}

/// Commits fee conversion info paying the transaction fee in the chain's native fee asset at rate
/// 1/1, unless the account cannot read it.
///
/// Signature-based auth components abort when a non-zero `verification_base_fee` meets auth args
/// carrying no conversion info, so a request built without one is unexecutable rather than merely
/// suboptimal. Components that ignore the auth args settle their fee some other way and are left
/// alone, unless the request declares a salt such a component can never read (see
/// [`validate_fee_conversion_info_support`]).
///
/// The default salt is fixed because the signed transaction summary covers the auth args, and a
/// random salt would change the summary on every execution, breaking flows that reproduce one to
/// verify a signature over it. `AuthMultisig` is the mirror image: there the salt *is* the replay
/// guard, so the fixed one would eventually collide, and such a caller must declare a fresh one
/// with [`TransactionRequestBuilder::fee_conversion_salt`].
fn attach_native_fee_conversion_info(
    transaction_request: &mut TransactionRequest,
    account_code_interface: &AccountCodeInterface,
    reference_header: &BlockHeader,
) -> Result<(), ClientError> {
    // An auth arg the caller set is the caller's business: it may carry a commitment the caller
    // computed itself, or something else entirely. An empty word commits nothing, so it does not
    // count.
    if transaction_request.has_auth_arg() {
        return Ok(());
    }

    let fee_parameters = reference_header.fee_parameters();
    let declared_salt = transaction_request.fee_conversion_salt();
    if fee_parameters.verification_base_fee() == 0 && declared_salt.is_none() {
        return Ok(());
    }

    match FeeAuth::of(account_code_interface) {
        FeeAuth::FixedSalt => {
            transaction_request.commit_native_fee_conversion_info(
                fee_parameters.fee_faucet_id(),
                declared_salt.unwrap_or(NATIVE_FEE_CONVERSION_SALT),
            );
            Ok(())
        },
        FeeAuth::CallerChosenSalt(component) => match declared_salt {
            Some(salt) => {
                transaction_request
                    .commit_native_fee_conversion_info(fee_parameters.fee_faucet_id(), salt);
                Ok(())
            },
            None => Err(ClientError::TransactionRequestError(
                TransactionRequestError::FeeConversionInfoRequired(component),
            )),
        },
        FeeAuth::Ignored(component) => match declared_salt {
            // Batch execution skips `validate_account_request`, so the mismatch is caught here
            // too rather than silently dropping the declared salt.
            Some(_) => Err(ClientError::TransactionRequestError(
                TransactionRequestError::FeeConversionInfoUnsupported(component),
            )),
            None => Ok(()),
        },
    }
}

/// How an account's auth component treats the transaction's auth argument where a fee is charged.
enum FeeAuth {
    /// Reads it as fee conversion info without constraining the salt, so the client's fixed
    /// default salt works where the caller declares none.
    FixedSalt,
    /// Reads it as conversion info too, but reuses the salt as a replay guard the caller must
    /// choose. Carries the component's name for the error.
    CallerChosenSalt(String),
    /// Does not read it as conversion info, so anything written there is ignored and the fee is
    /// settled some other way. Carries the auth component's name, or `"unrecognized"` when the
    /// client recognizes no auth component at all.
    Ignored(String),
}

impl FeeAuth {
    /// Classifies the account's auth component.
    ///
    /// A single-sig component decides the answer wherever it sits in the component list, so the
    /// classification does not depend on the order components come back in.
    ///
    /// An unrecognized component is [`FeeAuth::Ignored`] and left alone: writing an argument such
    /// a component may read for its own purposes is worse than writing nothing. The component list
    /// is inspected directly because `AccountInterface::new` panics on exactly those components.
    fn of(account_code_interface: &AccountCodeInterface) -> Self {
        let procedures: Vec<_> = account_code_interface.procedures().iter().copied().collect();
        let components = AccountComponentInterface::from_procedures(&procedures);

        if components
            .iter()
            .any(|component| matches!(component, AccountComponentInterface::AuthSingleSig))
        {
            return Self::FixedSalt;
        }

        // Every multisig flavour whose MASM calls `fee::load_conversion_info` belongs here.
        // `multisig_smart.masm` and `guarded_multisig.masm` both `dupw` the auth argument, load the
        // conversion info out of it and keep the copy as the summary salt, so the salt is the
        // caller's replay guard in both.
        let caller_chosen_salt = components.iter().find_map(|component| match component {
            AccountComponentInterface::AuthMultisig
            | AccountComponentInterface::AuthMultisigSmart
            | AccountComponentInterface::AuthGuardedMultisig => {
                Some(Self::CallerChosenSalt(component.name()))
            },
            _ => None,
        });

        caller_chosen_salt.unwrap_or_else(|| {
            let name = components
                .iter()
                .find(|component| {
                    matches!(
                        component,
                        AccountComponentInterface::AuthNoAuth
                            | AccountComponentInterface::AuthNetworkAccount
                    )
                })
                .map_or_else(|| "unrecognized".into(), AccountComponentInterface::name);

            Self::Ignored(name)
        })
    }
}

/// Returns the conversion info an account should commit to settle its fee in the native asset at
/// rate 1/1, or `None` when the account pays its fee some other way.
///
/// Shared with note screening so the two cannot disagree about what an account needs.
pub(crate) fn native_fee_conversion_info(
    account_code_interface: &AccountCodeInterface,
    fee_parameters: &FeeParameters,
) -> Option<FeeConversionInfo> {
    if fee_parameters.verification_base_fee() == 0 {
        return None;
    }

    // Only a fixed salt can be paired with this info by anyone other than the caller: where the
    // salt is the account's replay guard, the caller is the one who has to choose it.
    match FeeAuth::of(account_code_interface) {
        FeeAuth::FixedSalt => Some(FeeConversionInfo::one_to_one(fee_parameters.fee_faucet_id())),
        FeeAuth::CallerChosenSalt(_) | FeeAuth::Ignored(_) => None,
    }
}

/// Verifies that the account can consume fee conversion info passed through the auth args.
///
/// Only the signature-based auth components read the auth args as conversion info (through
/// `miden::standards::fee`). On any other auth component the declared salt would go unread and no
/// conversion info would be committed, so the request is rejected here instead.
fn validate_fee_conversion_info_support(
    transaction_request: &TransactionRequest,
    account_code_interface: &AccountCodeInterface,
) -> Result<(), ClientError> {
    if transaction_request.fee_conversion_salt().is_none() {
        return Ok(());
    }

    match FeeAuth::of(account_code_interface) {
        FeeAuth::FixedSalt | FeeAuth::CallerChosenSalt(_) => Ok(()),
        FeeAuth::Ignored(auth_component) => Err(ClientError::TransactionRequestError(
            TransactionRequestError::FeeConversionInfoUnsupported(auth_component),
        )),
    }
}
/// Verifies that every output note emitted directly by the transaction declares `account_id` as
/// its sender.
///
/// A note's sender is bound by the kernel to the account that emits it, and note scripts (e.g.
/// P2IDE reclaim) authorize on that field, so an output note declaring a foreign sender can never
/// be executed. Catching it here yields a clear, immediate error instead of a cryptic failure deep
/// in transaction script building.
fn validate_output_note_senders(
    transaction_request: &TransactionRequest,
    account_id: AccountId,
) -> Result<(), ClientError> {
    for note in transaction_request.expected_output_own_notes() {
        let sender = note.metadata().sender();
        if sender != account_id {
            return Err(ClientError::TransactionRequestError(
                TransactionRequestError::OutputNoteSenderMismatch {
                    expected: account_id,
                    actual: sender,
                },
            ));
        }
    }

    Ok(())
}

/// Ensures a transaction request is compatible with the account's committed vault assets,
/// primarily by checking asset balances against the requested transfers.
fn validate_basic_account_request(
    transaction_request: &TransactionRequest,
    vault_assets: &[Asset],
) -> Result<(), ClientError> {
    // Get outgoing assets
    let (fungible_balance_map, non_fungible_set) = get_outgoing_assets(transaction_request);

    // Get incoming assets
    let (incoming_fungible_balance_map, incoming_non_fungible_balance_set) =
        transaction_request.incoming_assets();

    // Aggregate the account's fungible balance per faucet in one pass. A faucet's fungible asset
    // may occupy more than one callback-flag vault key, so all matching entries are summed.
    let mut available_fungible: BTreeMap<AccountId, u64> = BTreeMap::new();
    for asset in vault_assets {
        if let Asset::Fungible(fungible) = asset {
            let balance = available_fungible.entry(fungible.faucet_id()).or_default();
            *balance = balance.saturating_add(fungible.amount().as_u64());
        }
    }

    // Check if the account balance plus incoming assets is greater than or equal to the
    // outgoing fungible assets
    for (faucet_id, amount) in fungible_balance_map {
        let account_asset_amount = available_fungible.get(&faucet_id).copied().unwrap_or(0);
        let incoming_balance = incoming_fungible_balance_map.get(&faucet_id).unwrap_or(&0);
        if account_asset_amount + incoming_balance < amount {
            return Err(ClientError::AssetError(AssetError::FungibleAssetAmountNotSufficient {
                minuend: account_asset_amount,
                subtrahend: amount,
            }));
        }
    }

    // Check if the account balance plus incoming assets is greater than or equal to the
    // outgoing non fungible assets
    for non_fungible in &non_fungible_set {
        let held = vault_assets
            .iter()
            .any(|asset| matches!(asset, Asset::NonFungible(nf) if nf == non_fungible));
        if !held && !incoming_non_fungible_balance_set.contains(non_fungible) {
            return Err(ClientError::TransactionRequestError(
                TransactionRequestError::MissingNonFungibleAsset(non_fungible.faucet_id()),
            ));
        }
    }

    Ok(())
}

/// Fetches a foreign account's proof and details from the network, converts them into
/// [`AccountInputs`], and caches the returned code in the store for future requests.
///
/// Storage maps the node caps as oversized (returned truncated) are carried root-only in the
/// inputs; reads from them resolve lazily as per-key witnesses during execution.
///
/// # Errors
/// Fails if the account is private: the RPC does not return account details for them, causing
/// [`TransactionRequestError::ForeignAccountDataMissing`].
pub(crate) async fn fetch_public_account_inputs(
    store: &Arc<dyn Store>,
    rpc_api: &Arc<dyn NodeRpcClient>,
    account_id: AccountId,
    storage_requirements: AccountStorageRequirements,
    account_state_at: AccountStateAt,
) -> Result<AccountInputs, ClientError> {
    let known_code: Option<AccountCode> =
        store.get_foreign_account_code(vec![account_id]).await?.into_values().next();

    // Tracked accounts skip the asset list when unchanged; untracked accounts fetch it in full
    // so asset reads need no execution-time RPC.
    let vault = store
        .get_account_header(account_id)
        .await?
        .map_or(VaultFetch::Always, |(header, ..)| {
            VaultFetch::IfChangedFrom(header.vault_root())
        });

    let (_block_num, account_proof) = rpc_api
        .get_account(
            account_id,
            GetAccountRequest::new()
                .with_storage(StorageMapFetch::Slots(storage_requirements.clone()))
                .at(account_state_at)
                .with_known_code(known_code)
                .with_vault(vault),
        )
        .await?;

    let account_inputs = request::account_proof_into_inputs(account_proof)?;

    let _ = store
        .upsert_foreign_account_code(account_id, account_inputs.code().clone())
        .await
        .inspect_err(|err| {
            tracing::warn!(
                %account_id,
                %err,
                "Failed to persist foreign account code to store"
            );
        });

    Ok(account_inputs)
}

/// Extracts notes from [`RawOutputNotes`].
/// Used for:
/// - Checking the relevance of notes to save them as input notes.
/// - Validate hashes versus expected output notes after a transaction is executed.
pub fn notes_from_output(output_notes: &RawOutputNotes) -> impl Iterator<Item = &Note> {
    output_notes.iter().filter_map(|n| match n {
        RawOutputNote::Full(n) => Some(n),
        RawOutputNote::Partial(_) => None,
    })
}

/// Validates that the executed transaction's output recipients match what was expected in the
/// transaction request.
pub(crate) fn validate_executed_transaction(
    executed_transaction: &ExecutedTransaction,
    expected_output_recipients: &[NoteRecipient],
) -> Result<(), ClientError> {
    let tx_output_recipient_digests = executed_transaction
        .output_notes()
        .iter()
        .filter_map(|n| n.recipient().map(NoteRecipient::digest))
        .collect::<Vec<_>>();

    let missing_recipient_digest: Vec<Word> = expected_output_recipients
        .iter()
        .filter_map(|recipient| {
            (!tx_output_recipient_digests.contains(&recipient.digest()))
                .then_some(recipient.digest())
        })
        .collect();

    if !missing_recipient_digest.is_empty() {
        return Err(ClientError::MissingOutputRecipients(missing_recipient_digest));
    }

    Ok(())
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::vec;

    use miden_protocol::Word;
    use miden_protocol::account::auth::AuthSecretKey;
    use miden_protocol::account::{
        Account,
        AccountBuilder,
        AccountComponent,
        AccountComponentMetadata,
        AccountId,
        AccountType,
    };
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::block::{BlockHeader, BlockNumber, FeeParameters};
    use miden_protocol::crypto::rand::RandomCoin;
    use miden_protocol::note::{Note, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_protocol::testing::validator_keys::random_validator_set;
    use miden_standards::account::AccountBuilderSchemaCommitmentExt;
    use miden_standards::account::auth::{
        Approver,
        ApproverSet,
        AuthGuardedMultisig,
        AuthGuardedMultisigConfig,
        AuthMultisig,
        AuthMultisigConfig,
        AuthMultisigSmart,
        AuthMultisigSmartConfig,
        AuthSingleSig,
        FeeConversionInfo,
        GuardianConfig,
        NoAuth,
        commit_fee_conversion_info,
    };
    use miden_standards::account::wallets::BasicWallet;
    use miden_standards::note::P2idNote;

    use super::{
        AccountComponentInterface,
        NATIVE_FEE_CONVERSION_SALT,
        TransactionRequest,
        TransactionRequestBuilder,
        attach_native_fee_conversion_info,
        validate_fee_conversion_info_support,
        validate_output_note_senders,
    };
    use crate::ClientError;
    use crate::assembly::CodeBuilder;
    use crate::auth::AuthSchemeId;
    use crate::transaction::TransactionRequestError;

    fn own_note_with_sender(sender: AccountId) -> Note {
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let target_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let mut rng = RandomCoin::new(Word::default());

        P2idNote::builder()
            .sender(sender)
            .target(target_id)
            .asset(FungibleAsset::new(faucet_id, 100).unwrap())
            .note_type(NoteType::Public)
            .generate_serial_number(&mut rng)
            .build()
            .expect("note creation failed")
            .into()
    }

    #[test]
    fn output_note_with_foreign_sender_is_rejected() {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let foreign_sender = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        assert_ne!(account_id, foreign_sender);

        let request = TransactionRequestBuilder::new()
            .own_output_notes(vec![own_note_with_sender(foreign_sender)])
            .build()
            .unwrap();

        let err = validate_output_note_senders(&request, account_id).unwrap_err();
        match err {
            ClientError::TransactionRequestError(
                TransactionRequestError::OutputNoteSenderMismatch { expected, actual },
            ) => {
                assert_eq!(expected, account_id);
                assert_eq!(actual, foreign_sender);
            },
            other => panic!("expected OutputNoteSenderMismatch, got {other:?}"),
        }
    }

    #[test]
    fn output_note_with_matching_sender_is_accepted() {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();

        let request = TransactionRequestBuilder::new()
            .own_output_notes(vec![own_note_with_sender(account_id)])
            .build()
            .unwrap();

        validate_output_note_senders(&request, account_id).unwrap();
    }

    #[test]
    fn request_without_own_output_notes_is_accepted() {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();

        // A consume-only request (input note, no own output notes) must pass the sender check.
        let request = TransactionRequestBuilder::new()
            .input_notes(vec![(own_note_with_sender(faucet_id), None)])
            .build()
            .unwrap();

        validate_output_note_senders(&request, account_id).unwrap();
    }

    /// Builds an account carrying `auth_component` and a basic wallet.
    fn account_with_auth(auth_component: impl Into<AccountComponent>) -> Account {
        AccountBuilder::new([7u8; 32])
            .account_type(AccountType::Public)
            .with_component(auth_component)
            .with_component(BasicWallet)
            .build_with_schema_commitment()
            .expect("account creation failed")
    }

    fn fee_conversion_request() -> TransactionRequest {
        TransactionRequestBuilder::new()
            .fee_conversion_salt(Word::from([13u32, 14, 15, 16]))
            .build()
            .unwrap()
    }

    #[test]
    fn fee_conversion_info_is_accepted_by_a_signature_authenticated_account() {
        let key = AuthSecretKey::new_falcon512_poseidon2();
        let auth = AuthSingleSig::new(Approver::new(
            key.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        ));

        validate_fee_conversion_info_support(
            &fee_conversion_request(),
            &account_with_auth(auth).code_interface(),
        )
        .unwrap();
    }

    #[test]
    fn fee_conversion_info_is_rejected_by_an_account_that_cannot_read_it() {
        let account = account_with_auth(NoAuth);

        let err = validate_fee_conversion_info_support(
            &fee_conversion_request(),
            &account.code_interface(),
        )
        .expect_err("NoAuth does not read the auth args");
        match err {
            ClientError::TransactionRequestError(
                TransactionRequestError::FeeConversionInfoUnsupported(auth_component),
            ) => assert_eq!(auth_component, AccountComponentInterface::AuthNoAuth.name()),
            other => panic!("expected FeeConversionInfoUnsupported, got {other:?}"),
        }
    }

    #[test]
    fn a_request_without_fee_conversion_info_skips_the_auth_component_check() {
        // `NoAuth` cannot read conversion info, but a request that declares none is unaffected.
        validate_fee_conversion_info_support(
            &TransactionRequestBuilder::new().build().unwrap(),
            &account_with_auth(NoAuth).code_interface(),
        )
        .unwrap();
    }

    // NATIVE FEE CONVERSION INFO INJECTION
    // --------------------------------------------------------------------------------------------

    /// Fee faucet the headers below name, distinct from the faucet
    /// [`fee_conversion_request`] pays in so the two can be told apart.
    const NATIVE_FEE_FAUCET: u128 = ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;

    /// Builds a block header whose fee parameters charge `verification_base_fee` in
    /// [`NATIVE_FEE_FAUCET`]'s asset.
    fn header_with_base_fee(verification_base_fee: u32) -> BlockHeader {
        let fee_parameters = FeeParameters::new(
            AccountId::try_from(NATIVE_FEE_FAUCET).unwrap(),
            verification_base_fee,
        );
        let (_, validator_keys) = random_validator_set(1);

        BlockHeader::new(
            1,
            Word::empty(),
            BlockNumber::from(1u32),
            Word::empty(),
            Word::empty(),
            Word::empty(),
            Word::empty(),
            Word::empty(),
            Word::empty(),
            validator_keys,
            fee_parameters,
            0,
        )
    }

    /// Returns the auth arg a request carries once the native conversion info has been attached
    /// against a header charging `verification_base_fee`.
    fn injected_auth_arg(
        mut request: TransactionRequest,
        account: &Account,
        verification_base_fee: u32,
    ) -> Option<Word> {
        let _ = attach_native_fee_conversion_info(
            &mut request,
            &account.code_interface(),
            &header_with_base_fee(verification_base_fee),
        );
        *request.auth_arg()
    }

    /// As [`injected_auth_arg`], but surfaces the attachment error instead of discarding it.
    fn try_injected_auth_arg(
        mut request: TransactionRequest,
        account: &Account,
        verification_base_fee: u32,
    ) -> Result<Option<Word>, ClientError> {
        attach_native_fee_conversion_info(
            &mut request,
            &account.code_interface(),
            &header_with_base_fee(verification_base_fee),
        )?;
        Ok(*request.auth_arg())
    }

    fn singlesig_account() -> Account {
        let key = AuthSecretKey::new_falcon512_poseidon2();
        account_with_auth(AuthSingleSig::new(Approver::new(
            key.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
    }

    fn guarded_multisig_account() -> Account {
        let approvers = ApproverSet::new(
            vec![Approver::new(
                AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )],
            1,
        )
        .unwrap();

        let guardian = GuardianConfig::new(Approver::new(
            AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        ));

        account_with_auth(
            AuthGuardedMultisig::new(AuthGuardedMultisigConfig::new(approvers, guardian).unwrap())
                .unwrap(),
        )
    }

    #[test]
    fn native_fee_conversion_info_is_attached_on_a_fee_charging_chain() {
        let auth_arg = injected_auth_arg(
            TransactionRequestBuilder::new().build().unwrap(),
            &singlesig_account(),
            500,
        )
        .expect("a fee-charging chain should get conversion info attached");

        let (expected, _) = commit_fee_conversion_info(
            FeeConversionInfo::one_to_one(AccountId::try_from(NATIVE_FEE_FAUCET).unwrap()),
            NATIVE_FEE_CONVERSION_SALT,
        );
        assert_eq!(auth_arg, expected, "the fee should be paid in the native asset at rate 1/1");
    }

    #[test]
    fn an_explicit_auth_arg_is_not_overwritten() {
        let auth_arg = Word::from([21u32, 22, 23, 24]);
        let request = TransactionRequestBuilder::new().auth_arg(auth_arg).build().unwrap();

        assert_eq!(
            injected_auth_arg(request, &singlesig_account(), 500),
            Some(auth_arg),
            "a request that declares its own auth arg keeps it"
        );
    }

    /// Expected commitment for the native 1/1 conversion info under `salt`.
    fn native_commitment(salt: Word) -> Word {
        let (auth_arg, _) = commit_fee_conversion_info(
            FeeConversionInfo::one_to_one(AccountId::try_from(NATIVE_FEE_FAUCET).unwrap()),
            salt,
        );
        auth_arg
    }

    #[test]
    fn a_declared_salt_is_used_for_the_native_commitment() {
        let salt = Word::from([17u32, 18, 19, 20]);
        let request = TransactionRequestBuilder::new().fee_conversion_salt(salt).build().unwrap();

        let auth_arg = injected_auth_arg(request, &singlesig_account(), 500)
            .expect("a declared salt should still get native conversion info attached");

        assert_eq!(auth_arg, native_commitment(salt));
    }

    fn multisig_account() -> Account {
        let approvers = ApproverSet::new(
            vec![Approver::new(
                AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )],
            1,
        )
        .unwrap();

        account_with_auth(AuthMultisig::new(AuthMultisigConfig::new(approvers)).unwrap())
    }

    #[test]
    fn nothing_is_attached_where_it_is_not_needed_or_not_readable() {
        for (case, account, base_fee) in [
            ("a zero base fee charges nothing", singlesig_account(), 0),
            ("NoAuth never reads the auth args", account_with_auth(NoAuth), 500),
            ("a multisig salt is its own replay guard", multisig_account(), 500),
        ] {
            assert_eq!(
                injected_auth_arg(
                    TransactionRequestBuilder::new().build().unwrap(),
                    &account,
                    base_fee
                ),
                None,
                "{case}"
            );
        }
    }

    // GUARDED MULTISIG
    // --------------------------------------------------------------------------------------------

    /// `guarded_multisig.masm` loads the conversion info out of the auth args and pays the fee with
    /// it, so a declared asset and rate are what the account pays with rather than something
    /// discarded and reinterpreted as the summary salt.
    #[test]
    fn fee_conversion_info_is_accepted_by_a_guarded_multisig_account() {
        validate_fee_conversion_info_support(
            &fee_conversion_request(),
            &guarded_multisig_account().code_interface(),
        )
        .expect("a guarded multisig reads the auth args as conversion info");
    }

    /// `guarded_multisig.masm` reuses the auth args as the summary salt after loading the
    /// conversion info out of them, exactly as `multisig.masm` does, so the same reasoning applies:
    /// the salt is the caller's replay guard and the client cannot pick it.
    #[test]
    fn a_guarded_multisig_account_must_declare_its_own_fee_conversion_info() {
        let err = try_injected_auth_arg(
            TransactionRequestBuilder::new().build().unwrap(),
            &guarded_multisig_account(),
            500,
        )
        .expect_err("a guarded multisig account cannot inherit the fixed native salt");
        match err {
            ClientError::TransactionRequestError(
                TransactionRequestError::FeeConversionInfoRequired(auth_component),
            ) => {
                assert_eq!(auth_component, AccountComponentInterface::AuthGuardedMultisig.name());
            },
            other => panic!("expected FeeConversionInfoRequired, got {other:?}"),
        }

        let salt = Word::from([13u32, 14, 15, 16]);
        assert_eq!(
            try_injected_auth_arg(fee_conversion_request(), &guarded_multisig_account(), 500)
                .expect("a declared salt is accepted"),
            Some(native_commitment(salt)),
            "a guarded multisig account that declares a salt commits the native conversion info"
        );

        assert_eq!(
            try_injected_auth_arg(
                TransactionRequestBuilder::new().build().unwrap(),
                &guarded_multisig_account(),
                0
            )
            .expect("a chain charging nothing needs no conversion info"),
            None,
        );
    }

    // SMART MULTISIG
    // --------------------------------------------------------------------------------------------

    fn smart_multisig_account() -> Account {
        let approvers = ApproverSet::new(
            vec![Approver::new(
                AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )],
            1,
        )
        .unwrap();

        account_with_auth(AuthMultisigSmart::new(AuthMultisigSmartConfig::new(approvers)).unwrap())
    }

    /// As of `0.16.0-rc.9` `multisig_smart.masm` loads the conversion info out of the auth args and
    /// pays the fee with it, exactly as `guarded_multisig.masm` does, so a declared asset and rate
    /// are what the account pays with rather than something discarded and reinterpreted as the
    /// summary salt.
    #[test]
    fn fee_conversion_info_is_accepted_by_a_smart_multisig_account() {
        validate_fee_conversion_info_support(
            &fee_conversion_request(),
            &smart_multisig_account().code_interface(),
        )
        .expect("a smart multisig reads the auth args as conversion info");
    }

    /// `multisig_smart.masm` reuses the auth args as the summary salt after loading the conversion
    /// info out of them, so the same reasoning as for the other multisig flavours applies: the salt
    /// is the caller's replay guard and the client cannot pick it.
    #[test]
    fn a_smart_multisig_account_must_declare_its_own_fee_conversion_info() {
        let err = try_injected_auth_arg(
            TransactionRequestBuilder::new().build().unwrap(),
            &smart_multisig_account(),
            500,
        )
        .expect_err("a smart multisig account cannot inherit the fixed native salt");
        match err {
            ClientError::TransactionRequestError(
                TransactionRequestError::FeeConversionInfoRequired(auth_component),
            ) => {
                assert_eq!(auth_component, AccountComponentInterface::AuthMultisigSmart.name());
            },
            other => panic!("expected FeeConversionInfoRequired, got {other:?}"),
        }

        let salt = Word::from([13u32, 14, 15, 16]);
        assert_eq!(
            try_injected_auth_arg(fee_conversion_request(), &smart_multisig_account(), 500)
                .expect("a declared salt is accepted"),
            Some(native_commitment(salt)),
            "a smart multisig account that declares a salt commits the native conversion info"
        );

        assert_eq!(
            try_injected_auth_arg(
                TransactionRequestBuilder::new().build().unwrap(),
                &smart_multisig_account(),
                0
            )
            .expect("a chain charging nothing needs no conversion info"),
            None,
        );
    }

    /// An account carrying a custom auth component names no recognized one, which
    /// `AccountInterface::new` asserts on rather than reports.
    #[test]
    fn an_unrecognized_auth_component_is_rejected_rather_than_panicking() {
        const CUSTOM_AUTH: &str = "
            use miden::protocol::native_account

            @auth_script
            pub proc auth_custom
                exec.native_account::incr_nonce
                drop
            end
        ";

        let code = CodeBuilder::default()
            .compile_component_code("miden::testing::custom_auth", CUSTOM_AUTH)
            .expect("custom auth component code should compile");
        let auth = AccountComponent::new(
            code,
            vec![],
            AccountComponentMetadata::new("miden::testing::custom_auth"),
        )
        .expect("custom auth component");

        let account = account_with_auth(auth);

        let err = validate_fee_conversion_info_support(
            &fee_conversion_request(),
            &account.code_interface(),
        )
        .expect_err("an account with no recognized auth component cannot read conversion info");
        assert!(matches!(
            err,
            ClientError::TransactionRequestError(
                TransactionRequestError::FeeConversionInfoUnsupported(_)
            )
        ));

        assert_eq!(
            try_injected_auth_arg(TransactionRequestBuilder::new().build().unwrap(), &account, 500)
                .expect("a request declaring nothing is left alone"),
            None,
            "an auth component nothing can reason about gets nothing attached"
        );
    }
}

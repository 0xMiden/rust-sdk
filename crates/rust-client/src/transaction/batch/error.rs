use alloc::boxed::Box;

use miden_protocol::block::BlockNumber;
use miden_protocol::note::NoteId;

use super::ProvenBatchSubmission;
use crate::rpc::RpcError;
use crate::store::StoreError;
use crate::transaction::TransactionStoreUpdateError;

/// Errors specific to `BatchBuilder` construction and operation.
#[derive(Debug, thiserror::Error)]
pub enum BatchBuilderError {
    /// A push consumed an input note that an earlier push in this batch already
    /// consumed. Guarded client-side to fail fast before hitting the node.
    #[error("input note {0} is already consumed by an earlier transaction in this batch")]
    DuplicateInputNote(NoteId),

    /// `submit` was called on a builder with zero successful pushes.
    #[error("batch is empty — push at least one transaction before submitting")]
    Empty,

    /// The batch submission came back without a definite outcome, so the node may or may not have
    /// accepted it. Nothing was recorded locally for any of its transactions.
    #[error(
        "submission of a batch of {} transactions came back without a definite outcome, so the \
         node may or may not have accepted it; nothing was recorded locally",
        submission.transaction_count()
    )]
    BatchSubmissionOutcomeUnknown {
        /// The batch as submitted, to resend with
        /// [`Client::submit_proven_batch`](crate::Client::submit_proven_batch) or to read
        /// `transaction_ids()` from and track.
        submission: Box<ProvenBatchSubmission>,
        #[source]
        source: RpcError,
    },

    /// The node accepted the batch (RPC returned `block_num`), but building one of the
    /// per-tx [`crate::transaction::TransactionStoreUpdate`]s failed. Callers should trigger
    /// `sync_state` to reconcile.
    #[error(
        "batch was accepted at block {block_num} but building store updates failed; sync_state to reconcile"
    )]
    BatchSubmittedButUpdateBuildFailed {
        block_num: BlockNumber,
        #[source]
        source: TransactionStoreUpdateError,
    },

    /// The node accepted the batch (RPC returned `block_num`), but applying
    /// the per-tx updates to the local store failed. Callers should trigger
    /// `sync_state` to reconcile.
    #[error(
        "batch was accepted at block {block_num} but applying to the store failed; sync_state to reconcile"
    )]
    BatchSubmittedButApplyFailed {
        block_num: BlockNumber,
        #[source]
        source: StoreError,
    },
}

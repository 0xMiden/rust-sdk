use miden_protocol::block::BlockNumber;
use miden_protocol::note::NoteId;

use crate::store::StoreError;
use crate::transaction::TransactionStoreUpdateError;

/// Errors specific to `BatchBuilder` construction and operation.
#[derive(Debug, thiserror::Error)]
pub enum BatchBuilderError {
    /// A push consumed an input note that an earlier push in this batch already consumed. Guarded
    /// client-side to fail fast before hitting the node.
    #[error("input note {0} is already consumed by an earlier transaction in this batch")]
    DuplicateInputNote(NoteId),

    /// `submit` was called on a builder with zero successful pushes.
    #[error("batch is empty — push at least one transaction before submitting")]
    Empty,

    /// The node accepted the batch (RPC returned `block_num`), but building one of the per-tx
    /// [`crate::transaction::TransactionStoreUpdate`]s failed. Callers should trigger `sync_state`
    /// to reconcile.
    #[error(
        "batch was accepted at block {block_num} but building store updates failed; sync_state to reconcile"
    )]
    BatchSubmittedButUpdateBuildFailed {
        block_num: BlockNumber,
        #[source]
        source: TransactionStoreUpdateError,
    },

    /// The node accepted the batch (RPC returned `block_num`), but applying the per-tx updates to
    /// the local store failed. Callers should trigger `sync_state` to reconcile.
    #[error(
        "batch was accepted at block {block_num} but applying to the store failed; sync_state to reconcile"
    )]
    BatchSubmittedButApplyFailed {
        block_num: BlockNumber,
        #[source]
        source: StoreError,
    },
}

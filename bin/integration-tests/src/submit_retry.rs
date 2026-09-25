//! An RPC client which resubmits a transaction the node rejects because it consumes an
//! unauthenticated note that the node does not know yet.
//!
//! The node's funding service answers a request with the funding note before it submits the
//! transaction which creates that note. A test which consumes the note as an unauthenticated input
//! can therefore submit its own transaction first. The node rejects that transaction, and accepts
//! the same proven transaction once the funding transaction reaches it. Resubmitting the proven
//! transaction avoids a second proof and a wait for the funding transaction to commit.

use std::collections::BTreeSet;
use std::error::Error;
use std::time::{Duration, Instant};

use miden_client::rpc::domain::account::{AccountProof, GetAccountRequest};
use miden_client::rpc::domain::account_vault::AccountVaultInfo;
use miden_client::rpc::domain::note::{FetchedNote, SyncNotesBlock};
use miden_client::rpc::domain::nullifier::NullifierUpdate;
use miden_client::rpc::domain::storage_map::StorageMapInfo;
use miden_client::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
use miden_client::rpc::domain::transaction::TransactionRecord;
use miden_client::rpc::encryption::{AttestedTransactionEncryptionKey, SealedTransactionInputs};
use miden_client::rpc::{NetworkNoteStatusInfo, NodeRpcClient, RpcError, RpcLimits, RpcStatusInfo};
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::address::NetworkId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::{BlockHeader, BlockNumber, SignedBlock};
use miden_protocol::crypto::merkle::mmr::MmrProof;
use miden_protocol::note::{NoteId, NoteScript, NoteTag};
use miden_protocol::transaction::ProvenTransaction;
use miden_protocol::vm::ExecutionProof;

// RETRYING SUBMISSION CLIENT
// ================================================================================================

/// The message with which the node rejects a submission that consumes an unknown unauthenticated
/// note.
const UNKNOWN_UNAUTHENTICATED_NOTES: &str = "unauthenticated input notes are unknown";

/// How long a submission is retried before the rejection is returned.
///
/// It covers a funding service which still proves the funding transaction when the test submits.
const RETRY_DEADLINE: Duration = Duration::from_secs(120);

/// How long to wait between two submissions.
const RETRY_INTERVAL: Duration = Duration::from_millis(500);

/// Wraps an RPC client and resubmits a transaction or batch which the node rejects because it
/// consumes an unauthenticated note that the node does not know yet.
///
/// Every other request and every other error goes to the wrapped client unchanged.
pub struct UnknownNoteRetryRpcClient<T> {
    inner: T,
}

impl<T: NodeRpcClient> UnknownNoteRetryRpcClient<T> {
    /// Wraps `inner`.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

/// Calls `submit` until it succeeds or fails with an error other than an unknown unauthenticated
/// note. After `deadline` passes, the next unknown-note rejection is returned.
async fn retry_unknown_notes<F, Fut>(
    deadline: Duration,
    interval: Duration,
    submit: F,
) -> Result<BlockNumber, RpcError>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<BlockNumber, RpcError>>,
{
    let deadline = Instant::now() + deadline;
    loop {
        match submit().await {
            Err(err) if is_unknown_unauthenticated_note(&err) && Instant::now() < deadline => {
                tokio::time::sleep(interval).await;
            },
            result => return result,
        }
    }
}

/// Returns whether the node rejected a submission because it consumes an unauthenticated note that
/// the node does not know.
///
/// A transaction rejection shows the node's message in its own text. A batch rejection keeps the
/// message in its source, so the whole error chain is searched.
fn is_unknown_unauthenticated_note(err: &RpcError) -> bool {
    let mut source: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(err) = source {
        if err.to_string().contains(UNKNOWN_UNAUTHENTICATED_NOTES) {
            return true;
        }
        source = err.source();
    }
    false
}

#[async_trait::async_trait]
impl<T: NodeRpcClient> NodeRpcClient for UnknownNoteRetryRpcClient<T> {
    async fn submit_proven_transaction(
        &self,
        proven_transaction: &ProvenTransaction,
        sealed_transaction_inputs: SealedTransactionInputs,
    ) -> Result<BlockNumber, RpcError> {
        retry_unknown_notes(RETRY_DEADLINE, RETRY_INTERVAL, || {
            self.inner
                .submit_proven_transaction(proven_transaction, sealed_transaction_inputs.clone())
        })
        .await
    }

    async fn submit_proven_batch(
        &self,
        proven_batch: &ProvenBatch,
        proposed_batch: &ProposedBatch,
        sealed_transaction_inputs: Vec<SealedTransactionInputs>,
    ) -> Result<BlockNumber, RpcError> {
        retry_unknown_notes(RETRY_DEADLINE, RETRY_INTERVAL, || {
            self.inner.submit_proven_batch(
                proven_batch,
                proposed_batch,
                sealed_transaction_inputs.clone(),
            )
        })
        .await
    }

    async fn set_genesis_commitment(&self, commitment: Word) -> Result<(), RpcError> {
        self.inner.set_genesis_commitment(commitment).await
    }

    fn has_genesis_commitment(&self) -> Option<Word> {
        self.inner.has_genesis_commitment()
    }

    async fn get_transaction_encryption_key(
        &self,
    ) -> Result<AttestedTransactionEncryptionKey, RpcError> {
        self.inner.get_transaction_encryption_key().await
    }

    async fn get_block_header_by_number(
        &self,
        block_num: Option<BlockNumber>,
        include_mmr_proof: bool,
    ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
        self.inner.get_block_header_by_number(block_num, include_mmr_proof).await
    }

    async fn get_block_by_number(
        &self,
        block_num: BlockNumber,
        include_proof: bool,
    ) -> Result<(SignedBlock, Option<ExecutionProof>), RpcError> {
        self.inner.get_block_by_number(block_num, include_proof).await
    }

    async fn get_notes_by_id(&self, note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError> {
        self.inner.get_notes_by_id(note_ids).await
    }

    async fn sync_chain_mmr(
        &self,
        current_block_height: BlockNumber,
        upper_bound: SyncTarget,
    ) -> Result<ChainMmrInfo, RpcError> {
        self.inner.sync_chain_mmr(current_block_height, upper_bound).await
    }

    async fn sync_notes(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<SyncNotesBlock>, RpcError> {
        self.inner.sync_notes(block_from, block_to, note_tags).await
    }

    async fn sync_nullifiers(
        &self,
        prefix: &[u16],
        block_from: BlockNumber,
        block_to: BlockNumber,
    ) -> Result<Vec<NullifierUpdate>, RpcError> {
        self.inner.sync_nullifiers(prefix, block_from, block_to).await
    }

    async fn get_account(
        &self,
        account_id: AccountId,
        request: GetAccountRequest,
    ) -> Result<(BlockNumber, AccountProof), RpcError> {
        self.inner.get_account(account_id, request).await
    }

    async fn register_account(
        &self,
        invitation_code: &str,
        account_id: AccountId,
    ) -> Result<(), RpcError> {
        self.inner.register_account(invitation_code, account_id).await
    }

    async fn is_account_allowed(&self, account_id: AccountId) -> Result<bool, RpcError> {
        self.inner.is_account_allowed(account_id).await
    }

    async fn get_note_script_by_root(&self, root: Word) -> Result<Option<NoteScript>, RpcError> {
        self.inner.get_note_script_by_root(root).await
    }

    async fn sync_storage_maps(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<StorageMapInfo, RpcError> {
        self.inner.sync_storage_maps(block_from, block_to, account_id).await
    }

    async fn sync_account_vault(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<AccountVaultInfo, RpcError> {
        self.inner.sync_account_vault(block_from, block_to, account_id).await
    }

    async fn sync_transactions(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: Vec<AccountId>,
    ) -> Result<Vec<TransactionRecord>, RpcError> {
        self.inner.sync_transactions(block_from, block_to, account_ids).await
    }

    async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
        self.inner.get_network_id().await
    }

    async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError> {
        self.inner.get_rpc_limits().await
    }

    fn has_rpc_limits(&self) -> Option<RpcLimits> {
        self.inner.has_rpc_limits()
    }

    async fn set_rpc_limits(&self, limits: RpcLimits) {
        self.inner.set_rpc_limits(limits).await;
    }

    async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
        self.inner.get_status_unversioned().await
    }

    async fn get_network_note_status(
        &self,
        note_id: NoteId,
    ) -> Result<NetworkNoteStatusInfo, RpcError> {
        self.inner.get_network_note_status(note_id).await
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use miden_client::rpc::{AddTransactionError, EndpointError, GrpcError, RpcEndpoint};

    use super::*;

    /// The rejection of a transaction submission, which carries the typed endpoint error.
    fn transaction_rejection(error: AddTransactionError) -> RpcError {
        RpcError::RequestError {
            endpoint: RpcEndpoint::SubmitProvenTx,
            error_kind: GrpcError::InvalidArgument,
            endpoint_error: Some(EndpointError::AddTransaction(error)),
            source: None,
        }
    }

    fn unknown_notes_rejection() -> RpcError {
        transaction_rejection(AddTransactionError::StateConflict {
            message: format!("{UNKNOWN_UNAUTHENTICATED_NOTES}: [0x01]"),
        })
    }

    /// The rejection of a batch submission, which carries only the node's message.
    fn batch_rejection(message: &str) -> RpcError {
        RpcError::RequestError {
            endpoint: RpcEndpoint::SubmitProvenBatch,
            error_kind: GrpcError::InvalidArgument,
            endpoint_error: None,
            source: Some(Box::new(std::io::Error::other(message.to_string()))),
        }
    }

    #[test]
    fn only_unknown_note_rejections_are_recognized() {
        assert!(is_unknown_unauthenticated_note(&unknown_notes_rejection()));
        assert!(is_unknown_unauthenticated_note(&batch_rejection(&format!(
            "{UNKNOWN_UNAUTHENTICATED_NOTES}: [0x01]"
        ))));

        assert!(!is_unknown_unauthenticated_note(&transaction_rejection(
            AddTransactionError::StateConflict {
                message: "nullifiers already exist".into()
            }
        )));
        assert!(!is_unknown_unauthenticated_note(&transaction_rejection(
            AddTransactionError::Expired
        )));
        assert!(!is_unknown_unauthenticated_note(&batch_rejection("mempool is full")));
        assert!(!is_unknown_unauthenticated_note(&RpcError::InvalidResponse(
            "unexpected block".into()
        )));
    }

    #[tokio::test]
    async fn a_submission_is_repeated_until_the_node_knows_the_notes() {
        let calls = AtomicUsize::new(0);

        let result = retry_unknown_notes(Duration::from_secs(10), Duration::ZERO, || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                Err(unknown_notes_rejection())
            } else {
                Ok(BlockNumber::from(7u32))
            }
        })
        .await;

        assert_eq!(result.unwrap(), BlockNumber::from(7u32));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn another_error_is_returned_without_a_retry() {
        let calls = AtomicUsize::new(0);

        let result = retry_unknown_notes(Duration::from_secs(10), Duration::ZERO, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(transaction_rejection(AddTransactionError::Expired))
        })
        .await;

        assert!(matches!(
            result.unwrap_err().endpoint_error(),
            Some(EndpointError::AddTransaction(AddTransactionError::Expired))
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// A funding transaction which never reaches the node must not hang the test forever.
    #[tokio::test]
    async fn the_rejection_is_returned_after_the_deadline() {
        let calls = AtomicUsize::new(0);

        let result = retry_unknown_notes(Duration::ZERO, Duration::ZERO, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(unknown_notes_rejection())
        })
        .await;

        assert!(is_unknown_unauthenticated_note(&result.unwrap_err()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

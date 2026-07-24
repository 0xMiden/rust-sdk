use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::string::ToString;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::address::NetworkId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::{BlockHeader, BlockNumber, ProvenBlock};
use miden_protocol::crypto::merkle::mmr::MmrProof;
use miden_protocol::note::{NoteId, NoteScript, NoteTag};
use miden_protocol::transaction::{ProvenTransaction, TransactionInputs};

use super::domain::account::{AccountProof, GetAccountRequest};
use super::domain::account_vault::AccountVaultInfo;
use super::domain::note::{CommittedNote, FetchedNote, SyncNotesBlock};
use super::domain::nullifier::NullifierUpdate;
use super::domain::storage_map::StorageMapInfo;
use super::domain::sync::{ChainMmrInfo, SyncTarget};
use super::domain::transaction::TransactionRecord;
use super::{
    AccountStateAt,
    NetworkNoteStatusInfo,
    NodeRpcClient,
    RpcError,
    RpcLimits,
    RpcStatusInfo,
};

// RESPONSE VERIFICATION HELPERS
// ================================================================================================

/// Returns [`RpcError::InvalidResponse`] if `requested` is `Some` and `returned` does not equal it.
fn verify_block_num(requested: Option<BlockNumber>, returned: BlockNumber) -> Result<(), RpcError> {
    if let Some(requested) = requested
        && returned != requested
    {
        return Err(RpcError::InvalidResponse(format!(
            "node returned block {returned} but block {requested} was requested"
        )));
    }
    Ok(())
}

/// Returns [`RpcError::InvalidResponse`] if any returned note ID was not in `requested`.
fn verify_note_ids(
    requested: &BTreeSet<NoteId>,
    returned: impl IntoIterator<Item = NoteId>,
) -> Result<(), RpcError> {
    for id in returned {
        if !requested.contains(&id) {
            let list = requested.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            return Err(RpcError::InvalidResponse(format!(
                "node returned note {id} but [{list}] were requested"
            )));
        }
    }
    Ok(())
}

/// Returns [`RpcError::InvalidResponse`] if any returned note tag was not in `requested`.
fn verify_note_tags(
    requested: &BTreeSet<NoteTag>,
    returned: impl IntoIterator<Item = NoteTag>,
) -> Result<(), RpcError> {
    for tag in returned {
        if !requested.contains(&tag) {
            let list = requested.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
            return Err(RpcError::InvalidResponse(format!(
                "node returned note with tag {tag} but [{list}] were requested"
            )));
        }
    }
    Ok(())
}

/// Returns [`RpcError::InvalidResponse`] if any update carries a nullifier whose prefix was not in
/// `requested_prefixes`.
fn verify_nullifier_prefixes(
    requested_prefixes: &BTreeSet<u16>,
    batch: &[NullifierUpdate],
) -> Result<(), RpcError> {
    for update in batch {
        let prefix = update.nullifier.prefix();
        if !requested_prefixes.contains(&prefix) {
            let requested = requested_prefixes
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(RpcError::InvalidResponse(format!(
                "node returned nullifier with prefix {prefix} but [{requested}] were requested"
            )));
        }
    }
    Ok(())
}

/// Returns [`RpcError::InvalidResponse`] if `script`'s root does not equal the `requested` root.
fn verify_note_script_root(requested: Word, script: &NoteScript) -> Result<(), RpcError> {
    let fetched_root = script.root();
    if Word::from(fetched_root) != requested {
        return Err(RpcError::InvalidResponse(format!(
            "node returned note script with root {fetched_root} for requested root {requested}"
        )));
    }
    Ok(())
}

// VERIFYING RPC CLIENT
// ================================================================================================

/// A [`NodeRpcClient`] wrapper that verifies that responses correspond to the method's arguments,
/// rejecting mismatches with [`RpcError::InvalidResponse`]:
///
/// - [`get_block_header_by_number`](NodeRpcClient::get_block_header_by_number) and
///   [`get_block_by_number`](NodeRpcClient::get_block_by_number): the returned block's number must
///   match the requested one.
/// - [`get_notes_by_id`](NodeRpcClient::get_notes_by_id): every returned note's ID must have been
///   requested.
/// - [`sync_notes`](NodeRpcClient::sync_notes): every returned note's tag must have been requested.
/// - [`sync_nullifiers`](NodeRpcClient::sync_nullifiers): every returned nullifier's prefix must
///   have been requested.
/// - [`get_account`](NodeRpcClient::get_account): when the state at a specific block was requested,
///   the response must be for that block.
/// - [`get_note_script_by_root`](NodeRpcClient::get_note_script_by_root): a returned script's root
///   must match the requested one.
///
/// All other methods delegate to the wrapped client unchanged.
pub struct VerifyingRpcClient<T>(T);

impl<T: NodeRpcClient> VerifyingRpcClient<T> {
    /// Wraps `client` so that its responses are verified against the request.
    pub fn new(client: T) -> Self {
        Self(client)
    }
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
impl<T: NodeRpcClient> NodeRpcClient for VerifyingRpcClient<T> {
    async fn set_genesis_commitment(&self, commitment: Word) -> Result<(), RpcError> {
        self.0.set_genesis_commitment(commitment).await
    }

    fn has_genesis_commitment(&self) -> Option<Word> {
        self.0.has_genesis_commitment()
    }

    async fn submit_proven_transaction(
        &self,
        proven_transaction: ProvenTransaction,
        transaction_inputs: TransactionInputs,
    ) -> Result<BlockNumber, RpcError> {
        self.0.submit_proven_transaction(proven_transaction, transaction_inputs).await
    }

    async fn submit_proven_batch(
        &self,
        proven_batch: ProvenBatch,
        proposed_batch: ProposedBatch,
        transaction_inputs: Vec<TransactionInputs>,
    ) -> Result<BlockNumber, RpcError> {
        self.0
            .submit_proven_batch(proven_batch, proposed_batch, transaction_inputs)
            .await
    }

    async fn get_block_header_by_number(
        &self,
        block_num: Option<BlockNumber>,
        include_mmr_proof: bool,
    ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
        let (header, mmr_proof) =
            self.0.get_block_header_by_number(block_num, include_mmr_proof).await?;
        verify_block_num(block_num, header.block_num())?;
        Ok((header, mmr_proof))
    }

    async fn get_block_by_number(
        &self,
        block_num: BlockNumber,
        include_proof: bool,
    ) -> Result<ProvenBlock, RpcError> {
        let block = self.0.get_block_by_number(block_num, include_proof).await?;
        verify_block_num(Some(block_num), block.header().block_num())?;
        Ok(block)
    }

    async fn get_notes_by_id(&self, note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError> {
        let notes = self.0.get_notes_by_id(note_ids).await?;
        let requested: BTreeSet<NoteId> = note_ids.iter().copied().collect();
        verify_note_ids(&requested, notes.iter().map(FetchedNote::id))?;
        Ok(notes)
    }

    async fn sync_chain_mmr(
        &self,
        current_block_height: BlockNumber,
        upper_bound: SyncTarget,
    ) -> Result<ChainMmrInfo, RpcError> {
        self.0.sync_chain_mmr(current_block_height, upper_bound).await
    }

    async fn sync_notes(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<SyncNotesBlock>, RpcError> {
        let blocks = self.0.sync_notes(block_from, block_to, note_tags).await?;
        verify_note_tags(
            note_tags,
            blocks.iter().flat_map(|block| block.notes.values().map(CommittedNote::tag)),
        )?;
        Ok(blocks)
    }

    async fn sync_nullifiers(
        &self,
        prefix: &[u16],
        block_from: BlockNumber,
        block_to: BlockNumber,
    ) -> Result<Vec<NullifierUpdate>, RpcError> {
        let nullifiers = self.0.sync_nullifiers(prefix, block_from, block_to).await?;
        let requested: BTreeSet<u16> = prefix.iter().copied().collect();
        verify_nullifier_prefixes(&requested, &nullifiers)?;
        Ok(nullifiers)
    }

    async fn get_account(
        &self,
        account_id: AccountId,
        request: GetAccountRequest,
    ) -> Result<(BlockNumber, AccountProof), RpcError> {
        let requested = match request.at {
            AccountStateAt::Block(number) => Some(number),
            AccountStateAt::ChainTip => None,
        };
        let (block_num, proof) = self.0.get_account(account_id, request).await?;
        verify_block_num(requested, block_num)?;
        Ok((block_num, proof))
    }

    async fn get_note_script_by_root(&self, root: Word) -> Result<Option<NoteScript>, RpcError> {
        let script = self.0.get_note_script_by_root(root).await?;
        if let Some(script) = &script {
            verify_note_script_root(root, script)?;
        }
        Ok(script)
    }

    async fn sync_storage_maps(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<StorageMapInfo, RpcError> {
        self.0.sync_storage_maps(block_from, block_to, account_id).await
    }

    async fn sync_account_vault(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<AccountVaultInfo, RpcError> {
        self.0.sync_account_vault(block_from, block_to, account_id).await
    }

    async fn sync_transactions(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: Vec<AccountId>,
    ) -> Result<Vec<TransactionRecord>, RpcError> {
        self.0.sync_transactions(block_from, block_to, account_ids).await
    }

    async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
        self.0.get_network_id().await
    }

    async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError> {
        self.0.get_rpc_limits().await
    }

    fn has_rpc_limits(&self) -> Option<RpcLimits> {
        self.0.has_rpc_limits()
    }

    async fn set_rpc_limits(&self, limits: RpcLimits) {
        self.0.set_rpc_limits(limits).await;
    }

    async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
        self.0.get_status_unversioned().await
    }

    async fn get_network_note_status(
        &self,
        note_id: NoteId,
    ) -> Result<NetworkNoteStatusInfo, RpcError> {
        self.0.get_network_note_status(note_id).await
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use core::slice;
    use std::boxed::Box;
    use std::collections::BTreeSet;
    use std::vec::Vec;

    use miden_protocol::account::AccountId;
    use miden_protocol::address::NetworkId;
    use miden_protocol::batch::{ProposedBatch, ProvenBatch};
    use miden_protocol::block::{BlockHeader, BlockNumber, ProvenBlock};
    use miden_protocol::crypto::merkle::mmr::MmrProof;
    use miden_protocol::note::{NoteId, NoteScript, NoteTag, Nullifier};
    use miden_protocol::transaction::{ProvenTransaction, TransactionInputs};
    use miden_protocol::{Felt, Word};

    use super::{VerifyingRpcClient, verify_note_ids, verify_note_tags, verify_nullifier_prefixes};
    use crate::rpc::domain::account::{AccountProof, GetAccountRequest};
    use crate::rpc::domain::account_vault::AccountVaultInfo;
    use crate::rpc::domain::note::{FetchedNote, SyncNotesBlock};
    use crate::rpc::domain::nullifier::NullifierUpdate;
    use crate::rpc::domain::storage_map::StorageMapInfo;
    use crate::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
    use crate::rpc::domain::transaction::TransactionRecord;
    use crate::rpc::{NetworkNoteStatusInfo, NodeRpcClient, RpcError, RpcLimits, RpcStatusInfo};

    fn nullifier_with_prefix(prefix: u16) -> Nullifier {
        Nullifier::from_raw(Word::new([
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::new_unchecked(u64::from(prefix) << 48),
        ]))
    }

    #[test]
    fn verify_nullifier_prefixes_rejects_unrequested() {
        let requested = NullifierUpdate {
            nullifier: nullifier_with_prefix(0x1234),
            block_num: 1u32.into(),
        };
        let unrequested = NullifierUpdate {
            nullifier: nullifier_with_prefix(0xabcd),
            block_num: 2u32.into(),
        };

        let requested_prefixes: BTreeSet<u16> = BTreeSet::from([0x1234]);

        verify_nullifier_prefixes(&requested_prefixes, slice::from_ref(&requested))
            .expect("requested prefix must be accepted");

        let err = verify_nullifier_prefixes(&requested_prefixes, &[requested, unrequested])
            .expect_err("unrequested prefix must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[test]
    fn verify_note_tags_rejects_unrequested() {
        let requested = NoteTag::new(1);
        let other = NoteTag::new(2);
        let requested_set = BTreeSet::from([requested]);

        verify_note_tags(&requested_set, [requested]).expect("requested tag must be accepted");

        let err = verify_note_tags(&requested_set, [other])
            .expect_err("unrequested tag must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    fn note_id(n: u32) -> NoteId {
        NoteId::from_raw(Word::from([n, 0, 0, 0]))
    }

    #[test]
    fn verify_note_ids_rejects_unrequested() {
        let requested = note_id(1);
        let other = note_id(2);
        let requested_set = BTreeSet::from([requested]);

        verify_note_ids(&requested_set, [requested]).expect("requested note id must be accepted");

        let err = verify_note_ids(&requested_set, [other])
            .expect_err("unrequested note id must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    /// A transport whose `sync_nullifiers` always returns a fixed batch, regardless of the
    /// requested prefixes. Every other method is unreachable in these tests.
    struct FixedNullifiersTransport(Vec<NullifierUpdate>);

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl NodeRpcClient for FixedNullifiersTransport {
        async fn set_genesis_commitment(&self, _commitment: Word) -> Result<(), RpcError> {
            unimplemented!()
        }

        fn has_genesis_commitment(&self) -> Option<Word> {
            unimplemented!()
        }

        async fn submit_proven_transaction(
            &self,
            _proven_transaction: ProvenTransaction,
            _transaction_inputs: TransactionInputs,
        ) -> Result<BlockNumber, RpcError> {
            unimplemented!()
        }

        async fn submit_proven_batch(
            &self,
            _proven_batch: ProvenBatch,
            _proposed_batch: ProposedBatch,
            _transaction_inputs: Vec<TransactionInputs>,
        ) -> Result<BlockNumber, RpcError> {
            unimplemented!()
        }

        async fn get_block_header_by_number(
            &self,
            _block_num: Option<BlockNumber>,
            _include_mmr_proof: bool,
        ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
            unimplemented!()
        }

        async fn get_block_by_number(
            &self,
            _block_num: BlockNumber,
            _include_proof: bool,
        ) -> Result<ProvenBlock, RpcError> {
            unimplemented!()
        }

        async fn get_notes_by_id(
            &self,
            _note_ids: &[NoteId],
        ) -> Result<Vec<FetchedNote>, RpcError> {
            unimplemented!()
        }

        async fn sync_chain_mmr(
            &self,
            _current_block_height: BlockNumber,
            _upper_bound: SyncTarget,
        ) -> Result<ChainMmrInfo, RpcError> {
            unimplemented!()
        }

        async fn sync_notes(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _note_tags: &BTreeSet<NoteTag>,
        ) -> Result<Vec<SyncNotesBlock>, RpcError> {
            unimplemented!()
        }

        async fn sync_nullifiers(
            &self,
            _prefix: &[u16],
            _block_from: BlockNumber,
            _block_to: BlockNumber,
        ) -> Result<Vec<NullifierUpdate>, RpcError> {
            Ok(self.0.clone())
        }

        async fn get_account(
            &self,
            _account_id: AccountId,
            _request: GetAccountRequest,
        ) -> Result<(BlockNumber, AccountProof), RpcError> {
            unimplemented!()
        }

        async fn get_note_script_by_root(
            &self,
            _root: Word,
        ) -> Result<Option<NoteScript>, RpcError> {
            unimplemented!()
        }

        async fn sync_storage_maps(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _account_id: AccountId,
        ) -> Result<StorageMapInfo, RpcError> {
            unimplemented!()
        }

        async fn sync_account_vault(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _account_id: AccountId,
        ) -> Result<AccountVaultInfo, RpcError> {
            unimplemented!()
        }

        async fn sync_transactions(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _account_ids: Vec<AccountId>,
        ) -> Result<Vec<TransactionRecord>, RpcError> {
            unimplemented!()
        }

        async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
            unimplemented!()
        }

        async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError> {
            unimplemented!()
        }

        fn has_rpc_limits(&self) -> Option<RpcLimits> {
            unimplemented!()
        }

        async fn set_rpc_limits(&self, _limits: RpcLimits) {
            unimplemented!()
        }

        async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
            unimplemented!()
        }

        async fn get_network_note_status(
            &self,
            _note_id: NoteId,
        ) -> Result<NetworkNoteStatusInfo, RpcError> {
            unimplemented!()
        }
    }

    #[tokio::test]
    async fn verifying_client_rejects_mismatched_responses() {
        let update = NullifierUpdate {
            nullifier: nullifier_with_prefix(0xabcd),
            block_num: 1u32.into(),
        };
        let client = VerifyingRpcClient::new(FixedNullifiersTransport(vec![update]));

        let nullifiers = client
            .sync_nullifiers(&[0xabcd], 0u32.into(), 1u32.into())
            .await
            .expect("requested prefix must be accepted");
        assert_eq!(nullifiers.len(), 1);

        let err = client
            .sync_nullifiers(&[0x1234], 0u32.into(), 1u32.into())
            .await
            .expect_err("unrequested prefix must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }
}

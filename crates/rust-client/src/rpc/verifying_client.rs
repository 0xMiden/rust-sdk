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
    use std::boxed::Box;
    use std::collections::BTreeSet;
    use std::string::String;
    use std::vec::Vec;

    use miden_protocol::account::AccountId;
    use miden_protocol::address::NetworkId;
    use miden_protocol::batch::{ProposedBatch, ProvenBatch};
    use miden_protocol::block::account_tree::AccountWitness;
    use miden_protocol::block::{
        BlockBody,
        BlockHeader,
        BlockNumber,
        BlockProof,
        BlockSignatures,
        ProvenBlock,
    };
    use miden_protocol::crypto::merkle::mmr::MmrProof;
    use miden_protocol::crypto::merkle::{MerklePath, SparseMerklePath};
    use miden_protocol::note::{
        NoteAttachments,
        NoteId,
        NoteInclusionProof,
        NoteMetadata,
        NoteScript,
        NoteTag,
        NoteType,
        Nullifier,
        PartialNoteMetadata,
    };
    use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;
    use miden_protocol::transaction::{
        OrderedTransactionHeaders,
        ProvenTransaction,
        TransactionInputs,
        TransactionKernel,
    };
    use miden_protocol::{Felt, Word};
    use miden_standards::note::StandardNote;

    use super::VerifyingRpcClient;
    use crate::rpc::domain::account::{AccountProof, GetAccountRequest};
    use crate::rpc::domain::account_vault::AccountVaultInfo;
    use crate::rpc::domain::note::{CommittedNote, FetchedNote, SyncNotesBlock};
    use crate::rpc::domain::nullifier::NullifierUpdate;
    use crate::rpc::domain::storage_map::StorageMapInfo;
    use crate::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
    use crate::rpc::domain::transaction::TransactionRecord;
    use crate::rpc::{
        AccountStateAt,
        NetworkNoteStatusInfo,
        NodeRpcClient,
        RpcError,
        RpcLimits,
        RpcStatusInfo,
    };

    // FIXTURES
    // --------------------------------------------------------------------------------------------

    fn test_account_id() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_SENDER).expect("test sender ID is well formed")
    }

    fn note_id(n: u32) -> NoteId {
        NoteId::from_raw(Word::from([n, 0, 0, 0]))
    }

    fn nullifier_with_prefix(prefix: u16) -> Nullifier {
        Nullifier::from_raw(Word::new([
            Felt::ZERO,
            Felt::ZERO,
            Felt::ZERO,
            Felt::new_unchecked(u64::from(prefix) << 48),
        ]))
    }

    fn nullifier_update(prefix: u16, block_num: u32) -> NullifierUpdate {
        NullifierUpdate {
            nullifier: nullifier_with_prefix(prefix),
            block_num: block_num.into(),
        }
    }

    fn block_header(block_num: u32) -> BlockHeader {
        BlockHeader::mock(block_num, None, None, &[], TransactionKernel.to_commitment())
    }

    fn proven_block(block_num: u32) -> ProvenBlock {
        let body = BlockBody::new_unchecked(
            Vec::new(),
            Vec::new(),
            Vec::new(),
            OrderedTransactionHeaders::new_unchecked(Vec::new()),
        );
        let signatures = BlockSignatures::new(Vec::new()).expect("no signatures is a valid set");

        ProvenBlock::new_unchecked(
            block_header(block_num),
            body,
            signatures,
            BlockProof::new_dummy(),
        )
    }

    fn inclusion_proof() -> NoteInclusionProof {
        let path =
            SparseMerklePath::from_parts(0, Vec::new()).expect("empty SparseMerklePath is valid");
        NoteInclusionProof::new(BlockNumber::GENESIS, 0, path)
            .expect("zero index is well below the per-block notes ceiling")
    }

    fn note_metadata(tag: NoteTag) -> NoteMetadata {
        NoteMetadata::new(
            PartialNoteMetadata::new(test_account_id(), NoteType::Public).with_tag(tag),
            &NoteAttachments::empty(),
        )
    }

    /// Wraps `note_id` in the shape `get_notes_by_id` responds with. The `Private` variant reports
    /// the ID it was handed instead of deriving it from the note's contents, so the surrounding
    /// fixtures do not constrain which ID a test can plant.
    fn fetched_note(note_id: NoteId) -> FetchedNote {
        FetchedNote::Private(
            note_id,
            note_metadata(NoteTag::new(0)),
            NoteAttachments::empty(),
            inclusion_proof(),
        )
    }

    fn sync_notes_block(block_num: u32, tags: &[NoteTag]) -> SyncNotesBlock {
        let notes = tags
            .iter()
            .enumerate()
            .map(|(index, tag)| {
                let id = note_id(u32::try_from(index).expect("test note count fits in a u32"));
                (id, CommittedNote::new(id, note_metadata(*tag), inclusion_proof()))
            })
            .collect();

        SyncNotesBlock {
            block_header: block_header(block_num),
            mmr_path: MerklePath::new(Vec::new()),
            notes,
        }
    }

    fn account_proof() -> AccountProof {
        let path = SparseMerklePath::from_parts(u64::MAX, Vec::new())
            .expect("an all-empty path spans the full account tree depth");
        let witness = AccountWitness::new(test_account_id(), Word::empty(), path)
            .expect("the path depth matches the account tree depth");

        AccountProof::new(witness, None)
            .expect("a proof without details has nothing to cross-check")
    }

    // CANNED TRANSPORT
    // --------------------------------------------------------------------------------------------

    /// The canned `get_note_script_by_root` response. An enum rather than a nested [`Option`] so
    /// that a test setting no response stays distinct from a node reporting no script for the
    /// requested root.
    #[derive(Default)]
    enum CannedScript {
        #[default]
        Unset,
        Absent,
        Present(NoteScript),
    }

    /// A transport that answers with canned responses regardless of the request, so that
    /// [`VerifyingRpcClient`] can be exercised against responses a well-behaved node would never
    /// produce. Ignoring the arguments is what lets a test drive one response into both an
    /// accepting and a rejecting request. Methods whose slot is left unset are unreachable: each
    /// test sets only what it exercises.
    #[derive(Default)]
    struct CannedTransport {
        block_header: Option<(BlockHeader, Option<MmrProof>)>,
        block: Option<ProvenBlock>,
        /// Note IDs to report from `get_notes_by_id`, wrapped into notes on each call because
        /// [`FetchedNote`] is not [`Clone`].
        note_ids: Option<Vec<NoteId>>,
        sync_notes: Option<Vec<SyncNotesBlock>>,
        nullifiers: Option<Vec<NullifierUpdate>>,
        account: Option<(BlockNumber, AccountProof)>,
        note_script: CannedScript,
        /// When set, every canned method fails instead of answering.
        fail_with: Option<String>,
    }

    impl CannedTransport {
        /// Returns the transport-level failure the test asked for, if any.
        fn failure(&self) -> Option<RpcError> {
            self.fail_with
                .as_ref()
                .map(|message| RpcError::ExpectedDataMissing(message.clone()))
        }

        fn canned<T: Clone>(
            &self,
            response: Option<&T>,
            missing: &'static str,
        ) -> Result<T, RpcError> {
            if let Some(err) = self.failure() {
                return Err(err);
            }
            Ok(response.cloned().expect(missing))
        }
    }

    #[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
    #[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
    impl NodeRpcClient for CannedTransport {
        async fn set_genesis_commitment(&self, _commitment: Word) -> Result<(), RpcError> {
            unimplemented!("not used in these tests")
        }

        fn has_genesis_commitment(&self) -> Option<Word> {
            unimplemented!("not used in these tests")
        }

        async fn submit_proven_transaction(
            &self,
            _proven_transaction: ProvenTransaction,
            _transaction_inputs: TransactionInputs,
        ) -> Result<BlockNumber, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn submit_proven_batch(
            &self,
            _proven_batch: ProvenBatch,
            _proposed_batch: ProposedBatch,
            _transaction_inputs: Vec<TransactionInputs>,
        ) -> Result<BlockNumber, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn get_block_header_by_number(
            &self,
            _block_num: Option<BlockNumber>,
            _include_mmr_proof: bool,
        ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
            self.canned(
                self.block_header.as_ref(),
                "test must set a canned get_block_header_by_number response",
            )
        }

        async fn get_block_by_number(
            &self,
            _block_num: BlockNumber,
            _include_proof: bool,
        ) -> Result<ProvenBlock, RpcError> {
            self.canned(self.block.as_ref(), "test must set a canned get_block_by_number response")
        }

        async fn get_notes_by_id(
            &self,
            _note_ids: &[NoteId],
        ) -> Result<Vec<FetchedNote>, RpcError> {
            let ids = self
                .canned(self.note_ids.as_ref(), "test must set canned get_notes_by_id note IDs")?;
            Ok(ids.into_iter().map(fetched_note).collect())
        }

        async fn sync_chain_mmr(
            &self,
            _current_block_height: BlockNumber,
            _upper_bound: SyncTarget,
        ) -> Result<ChainMmrInfo, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn sync_notes(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _note_tags: &BTreeSet<NoteTag>,
        ) -> Result<Vec<SyncNotesBlock>, RpcError> {
            self.canned(self.sync_notes.as_ref(), "test must set a canned sync_notes response")
        }

        async fn sync_nullifiers(
            &self,
            _prefix: &[u16],
            _block_from: BlockNumber,
            _block_to: BlockNumber,
        ) -> Result<Vec<NullifierUpdate>, RpcError> {
            self.canned(self.nullifiers.as_ref(), "test must set a canned sync_nullifiers response")
        }

        async fn get_account(
            &self,
            _account_id: AccountId,
            _request: GetAccountRequest,
        ) -> Result<(BlockNumber, AccountProof), RpcError> {
            self.canned(self.account.as_ref(), "test must set a canned get_account response")
        }

        async fn get_note_script_by_root(
            &self,
            _root: Word,
        ) -> Result<Option<NoteScript>, RpcError> {
            if let Some(err) = self.failure() {
                return Err(err);
            }
            match &self.note_script {
                CannedScript::Unset => {
                    panic!("test must set a canned get_note_script_by_root response")
                },
                CannedScript::Absent => Ok(None),
                CannedScript::Present(script) => Ok(Some(script.clone())),
            }
        }

        async fn sync_storage_maps(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _account_id: AccountId,
        ) -> Result<StorageMapInfo, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn sync_account_vault(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _account_id: AccountId,
        ) -> Result<AccountVaultInfo, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn sync_transactions(
            &self,
            _block_from: BlockNumber,
            _block_to: BlockNumber,
            _account_ids: Vec<AccountId>,
        ) -> Result<Vec<TransactionRecord>, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError> {
            unimplemented!("not used in these tests")
        }

        fn has_rpc_limits(&self) -> Option<RpcLimits> {
            unimplemented!("not used in these tests")
        }

        async fn set_rpc_limits(&self, _limits: RpcLimits) {
            unimplemented!("not used in these tests")
        }

        async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
            unimplemented!("not used in these tests")
        }

        async fn get_network_note_status(
            &self,
            _note_id: NoteId,
        ) -> Result<NetworkNoteStatusInfo, RpcError> {
            unimplemented!("not used in these tests")
        }
    }

    // TESTS
    // --------------------------------------------------------------------------------------------

    #[tokio::test]
    async fn get_block_header_by_number_verifies_block_num() {
        let client = VerifyingRpcClient::new(CannedTransport {
            block_header: Some((block_header(5), None)),
            ..Default::default()
        });

        let (header, _) = client
            .get_block_header_by_number(None, false)
            .await
            .expect("a chain tip request must accept a header for any block");
        assert_eq!(header.block_num(), BlockNumber::from(5u32));

        client
            .get_block_header_by_number(Some(BlockNumber::from(5u32)), false)
            .await
            .expect("a header for the requested block must be accepted");

        let err = client
            .get_block_header_by_number(Some(BlockNumber::from(6u32)), false)
            .await
            .expect_err("a header for another block must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn get_block_by_number_verifies_block_num() {
        let client = VerifyingRpcClient::new(CannedTransport {
            block: Some(proven_block(5)),
            ..Default::default()
        });

        let block = client
            .get_block_by_number(BlockNumber::from(5u32), false)
            .await
            .expect("the requested block must be accepted");
        assert_eq!(block.header().block_num(), BlockNumber::from(5u32));

        let err = client
            .get_block_by_number(BlockNumber::from(6u32), false)
            .await
            .expect_err("a block with another number must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn get_notes_by_id_verifies_note_ids() {
        let client = VerifyingRpcClient::new(CannedTransport {
            note_ids: Some(vec![note_id(1)]),
            ..Default::default()
        });

        let notes = client
            .get_notes_by_id(&[note_id(1)])
            .await
            .expect("the requested note must be accepted");
        assert_eq!(notes.len(), 1);

        // A requested note the node does not hold is simply absent from the response.
        client
            .get_notes_by_id(&[note_id(1), note_id(2)])
            .await
            .expect("a subset of the requested notes must be accepted");

        // `FetchedNote` is not `Debug`, so the rejections are unpacked instead of `expect_err`ed.
        let Err(err) = client.get_notes_by_id(&[note_id(2)]).await else {
            panic!("an unrequested note must be rejected")
        };
        assert!(matches!(err, RpcError::InvalidResponse(_)));

        let Err(err) = client.get_notes_by_id(&[]).await else {
            panic!("no note may come back when none were requested")
        };
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn get_notes_by_id_accepts_empty_and_repeated_responses() {
        let empty = VerifyingRpcClient::new(CannedTransport {
            note_ids: Some(Vec::new()),
            ..Default::default()
        });
        let notes = empty
            .get_notes_by_id(&[note_id(1)])
            .await
            .expect("an empty response must be accepted");
        assert!(notes.is_empty());

        // The check is membership only, so a node repeating a requested note is not rejected.
        let repeated = VerifyingRpcClient::new(CannedTransport {
            note_ids: Some(vec![note_id(1), note_id(1)]),
            ..Default::default()
        });
        let notes = repeated
            .get_notes_by_id(&[note_id(1)])
            .await
            .expect("a repeat of a requested note must be accepted");
        assert_eq!(notes.len(), 2);
    }

    #[tokio::test]
    async fn sync_notes_verifies_note_tags() {
        let requested = NoteTag::new(1);
        let other = NoteTag::new(2);
        let requested_tags = BTreeSet::from([requested]);

        let client = VerifyingRpcClient::new(CannedTransport {
            sync_notes: Some(vec![sync_notes_block(1, &[requested]), sync_notes_block(2, &[])]),
            ..Default::default()
        });
        let blocks = client
            .sync_notes(BlockNumber::GENESIS, BlockNumber::from(2u32), &requested_tags)
            .await
            .expect("requested tags and a block without notes must be accepted");
        assert_eq!(blocks.len(), 2);

        // The offending tag sits in the second block, so the check must span every returned block.
        let client = VerifyingRpcClient::new(CannedTransport {
            sync_notes: Some(vec![
                sync_notes_block(1, &[requested]),
                sync_notes_block(2, &[other]),
            ]),
            ..Default::default()
        });
        let err = client
            .sync_notes(BlockNumber::GENESIS, BlockNumber::from(2u32), &requested_tags)
            .await
            .expect_err("an unrequested tag must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn sync_nullifiers_verifies_prefixes() {
        let client = VerifyingRpcClient::new(CannedTransport {
            nullifiers: Some(vec![nullifier_update(0xabcd, 1)]),
            ..Default::default()
        });

        let nullifiers = client
            .sync_nullifiers(&[0xabcd], BlockNumber::GENESIS, BlockNumber::from(1u32))
            .await
            .expect("the requested prefix must be accepted");
        assert_eq!(nullifiers.len(), 1);

        let err = client
            .sync_nullifiers(&[0x1234], BlockNumber::GENESIS, BlockNumber::from(1u32))
            .await
            .expect_err("an unrequested prefix must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));

        let err = client
            .sync_nullifiers(&[], BlockNumber::GENESIS, BlockNumber::from(1u32))
            .await
            .expect_err("no nullifier may come back when no prefix was requested");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn get_account_verifies_block_num_only_for_pinned_requests() {
        let client = VerifyingRpcClient::new(CannedTransport {
            account: Some((BlockNumber::from(5u32), account_proof())),
            ..Default::default()
        });

        let (block_num, _) = client
            .get_account(test_account_id(), GetAccountRequest::new().at(AccountStateAt::ChainTip))
            .await
            .expect("a chain tip request must accept state at any block");
        assert_eq!(block_num, BlockNumber::from(5u32));

        client
            .get_account(
                test_account_id(),
                GetAccountRequest::new().at(AccountStateAt::Block(BlockNumber::from(5u32))),
            )
            .await
            .expect("state at the requested block must be accepted");

        let err = client
            .get_account(
                test_account_id(),
                GetAccountRequest::new().at(AccountStateAt::Block(BlockNumber::from(6u32))),
            )
            .await
            .expect_err("state at another block must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn get_note_script_by_root_verifies_script_root() {
        let script = StandardNote::P2ID.script();
        let root = Word::from(script.root());
        let other_script = StandardNote::SWAP.script();

        let absent = VerifyingRpcClient::new(CannedTransport {
            note_script: CannedScript::Absent,
            ..Default::default()
        });
        assert!(
            absent
                .get_note_script_by_root(root)
                .await
                .expect("an unregistered root must pass through")
                .is_none()
        );

        let client = VerifyingRpcClient::new(CannedTransport {
            note_script: CannedScript::Present(script),
            ..Default::default()
        });
        client
            .get_note_script_by_root(root)
            .await
            .expect("a script with the requested root must be accepted");

        let mismatched = VerifyingRpcClient::new(CannedTransport {
            note_script: CannedScript::Present(other_script),
            ..Default::default()
        });
        let err = mismatched
            .get_note_script_by_root(root)
            .await
            .expect_err("a script with another root must be rejected");
        assert!(matches!(err, RpcError::InvalidResponse(_)));
    }

    #[tokio::test]
    async fn transport_errors_pass_through_unchanged() {
        let client = VerifyingRpcClient::new(CannedTransport {
            fail_with: Some("BlockHeader".into()),
            ..Default::default()
        });

        let err = client
            .get_block_header_by_number(Some(BlockNumber::from(5u32)), false)
            .await
            .expect_err("the transport failure must surface");
        assert!(matches!(err, RpcError::ExpectedDataMissing(_)));
    }
}

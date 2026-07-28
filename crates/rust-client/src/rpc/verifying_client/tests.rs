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
use crate::rpc::encryption::{AttestedTransactionEncryptionKey, SealedTransactionInputs};
use crate::rpc::{
    AccountStateAt,
    NetworkNoteStatusInfo,
    NodeRpcClient,
    RpcError,
    RpcLimits,
    RpcStatusInfo,
};

// FIXTURES
// ================================================================================================

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

    ProvenBlock::new_unchecked(block_header(block_num), body, signatures, BlockProof::new_dummy())
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

    AccountProof::new(witness, None).expect("a proof without details has nothing to cross-check")
}

// CANNED TRANSPORT
// ================================================================================================

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

    fn canned<T: Clone>(&self, response: Option<&T>, missing: &'static str) -> Result<T, RpcError> {
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

    async fn get_transaction_encryption_key(
        &self,
    ) -> Result<AttestedTransactionEncryptionKey, RpcError> {
        unimplemented!("not used in these tests")
    }

    async fn submit_proven_transaction(
        &self,
        _proven_transaction: ProvenTransaction,
        _sealed_transaction_inputs: SealedTransactionInputs,
    ) -> Result<BlockNumber, RpcError> {
        unimplemented!("not used in these tests")
    }

    async fn submit_proven_batch(
        &self,
        _proven_batch: ProvenBatch,
        _proposed_batch: ProposedBatch,
        _transaction_inputs: Vec<SealedTransactionInputs>,
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

    async fn get_notes_by_id(&self, _note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError> {
        let ids =
            self.canned(self.note_ids.as_ref(), "test must set canned get_notes_by_id note IDs")?;
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

    async fn get_note_script_by_root(&self, _root: Word) -> Result<Option<NoteScript>, RpcError> {
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
// ================================================================================================

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
        sync_notes: Some(vec![sync_notes_block(1, &[requested]), sync_notes_block(2, &[other])]),
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

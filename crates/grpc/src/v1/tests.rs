use std::collections::BTreeSet;

use miden_client_core::errors::RpcError;
use miden_client_core::rpc::AccountStateAt;
use miden_client_core::rpc::domain::account::{AccountProof, GetAccountRequest};
use miden_client_core::rpc::domain::note::{CommittedNote, FetchedNote, SyncNotesBlock};
use miden_client_core::rpc::domain::nullifier::NullifierUpdate;
use miden_client_core::rpc::domain::transaction::TransactionRecord;
use miden_protocol::account::AccountId;
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::block::{BlockBody, BlockHeader, BlockNumber, BlockSignatures, SignedBlock};
use miden_protocol::crypto::merkle::{MerklePath, SparseMerklePath};
use miden_protocol::note::{
    NoteAttachments,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    NoteTag,
    NoteType,
    Nullifier,
    PartialNoteMetadata,
};
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
    ACCOUNT_ID_SENDER,
};
use miden_protocol::transaction::{InputNotes, OrderedTransactionHeaders, TransactionHeader};
use miden_protocol::{Felt, Word};
use miden_standards::note::StandardNote;

use super::{
    BlockHeaderResponse,
    GetAccount,
    GetBlockByNumber,
    GetBlockHeaderByNumber,
    GetNoteScriptByRoot,
    GetNotesById,
    SyncNotes,
    SyncNullifiers,
    SyncTransactions,
};
use crate::RpcMethod;

// FIXTURES
// ================================================================================================

fn test_account_id() -> AccountId {
    AccountId::try_from(ACCOUNT_ID_SENDER).expect("test sender ID is well formed")
}

fn other_account_id() -> AccountId {
    AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE)
        .expect("test account ID is well formed")
}

fn note_id(n: u32) -> NoteId {
    NoteId::from_raw(Word::from([n, 0, 0, 0]))
}

fn nullifier_update(prefix: u16, block_num: u32) -> NullifierUpdate {
    let nullifier = Nullifier::from_raw(Word::new([
        Felt::ZERO,
        Felt::ZERO,
        Felt::ZERO,
        Felt::new_unchecked(u64::from(prefix) << 48),
    ]));
    NullifierUpdate { nullifier, block_num: block_num.into() }
}

fn block_header(block_num: u32) -> BlockHeader {
    BlockHeader::mock(block_num, None, None, &[])
}

fn signed_block(block_num: u32) -> SignedBlock {
    let body = BlockBody::new_unchecked(
        Vec::new(),
        Vec::new(),
        Vec::new(),
        OrderedTransactionHeaders::new_unchecked(Vec::new()),
    );
    let signatures = BlockSignatures::new(Vec::new()).expect("no signatures is a valid set");

    SignedBlock::new_unchecked(block_header(block_num), body, signatures)
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

/// Builds the notes a `GetNotesById` response carries for `ids`. The `Private` variant reports the
/// ID it was handed instead of deriving it from the note's contents, so a test can plant any ID.
fn fetched_notes(ids: &[NoteId]) -> Vec<FetchedNote> {
    ids.iter()
        .map(|id| {
            FetchedNote::Private(
                *id,
                note_metadata(NoteTag::new(0)),
                NoteAttachments::empty(),
                inclusion_proof(),
            )
        })
        .collect()
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

fn transaction_record(account_id: AccountId) -> TransactionRecord {
    TransactionRecord::new(
        BlockNumber::GENESIS,
        TransactionHeader::new(
            account_id,
            Word::default(),
            Word::default(),
            InputNotes::new_unchecked(vec![]),
            vec![],
        )
        .unwrap(),
        vec![],
        vec![],
        vec![],
    )
}

fn account_proof() -> AccountProof {
    let path = SparseMerklePath::from_parts(u64::MAX, Vec::new())
        .expect("an all-empty path spans the full account tree depth");
    let witness = AccountWitness::new(test_account_id(), Word::empty(), path)
        .expect("the path depth matches the account tree depth");

    AccountProof::new(witness, None).expect("a proof without details has nothing to cross-check")
}

fn is_invalid_response(result: Result<(), RpcError>) -> bool {
    result.is_err_and(|err| matches!(err, RpcError::InvalidResponse(_)))
}

// TESTS
// ================================================================================================

#[test]
fn get_block_header_by_number_verifies_block_num() {
    let response = BlockHeaderResponse {
        block_header: block_header(5),
        mmr_proof: None,
        protocol_config: None,
    };
    let method = |block_num: Option<u32>| GetBlockHeaderByNumber {
        block_num: block_num.map(BlockNumber::from),
        include_mmr_proof: false,
        include_protocol_config: false,
    };

    method(None)
        .validate(&response)
        .expect("a chain tip request must accept a header for any block");
    method(Some(5))
        .validate(&response)
        .expect("a header for the requested block must be accepted");
    assert!(is_invalid_response(method(Some(6)).validate(&response)));
}

#[test]
fn get_block_by_number_verifies_block_num() {
    let response = (signed_block(5), None);
    let method = |block_num: u32| GetBlockByNumber {
        block_num: block_num.into(),
        include_proof: false,
    };

    method(5).validate(&response).expect("the requested block must be accepted");
    assert!(is_invalid_response(method(6).validate(&response)));
}

#[test]
fn get_notes_by_id_verifies_note_ids() {
    let response = fetched_notes(&[note_id(1)]);

    GetNotesById { note_ids: &[note_id(1)] }
        .validate(&response)
        .expect("the requested note must be accepted");

    // A requested note the node does not hold is simply absent from the response.
    GetNotesById { note_ids: &[note_id(1), note_id(2)] }
        .validate(&response)
        .expect("a subset of the requested notes must be accepted");

    assert!(is_invalid_response(
        GetNotesById { note_ids: &[note_id(2)] }.validate(&response)
    ));
    assert!(is_invalid_response(GetNotesById { note_ids: &[] }.validate(&response)));
}

#[test]
fn get_notes_by_id_accepts_empty_and_repeated_responses() {
    let method = GetNotesById { note_ids: &[note_id(1)] };

    method.validate(&Vec::new()).expect("an empty response must be accepted");

    // The check is membership only, so a node repeating a requested note is not rejected.
    method
        .validate(&fetched_notes(&[note_id(1), note_id(1)]))
        .expect("a repeat of a requested note must be accepted");
}

#[test]
fn sync_notes_verifies_note_tags() {
    let requested = NoteTag::new(1);
    let other = NoteTag::new(2);
    let note_tags = BTreeSet::from([requested]);
    let method = SyncNotes {
        block_from: BlockNumber::GENESIS,
        block_to: BlockNumber::from(2u32),
        note_tags: &note_tags,
    };

    method
        .validate(&vec![sync_notes_block(1, &[requested]), sync_notes_block(2, &[])])
        .expect("requested tags and a block without notes must be accepted");

    // The offending tag sits in the second block, so the check must span every returned block.
    let response = vec![sync_notes_block(1, &[requested]), sync_notes_block(2, &[other])];
    assert!(is_invalid_response(method.validate(&response)));
}

#[test]
fn sync_nullifiers_verifies_prefixes() {
    let response = vec![nullifier_update(0xabcd, 1)];
    let method = |prefixes| SyncNullifiers {
        prefixes,
        block_from: BlockNumber::GENESIS,
        block_to: BlockNumber::from(1u32),
    };

    method(&[0xabcd])
        .validate(&response)
        .expect("the requested prefix must be accepted");
    assert!(is_invalid_response(method(&[0x1234]).validate(&response)));
    assert!(is_invalid_response(method(&[]).validate(&response)));
}

#[test]
fn sync_nullifiers_verifies_block_range() {
    let method = SyncNullifiers {
        prefixes: &[0xabcd],
        block_from: BlockNumber::from(10u32),
        block_to: BlockNumber::from(20u32),
    };

    method
        .validate(&vec![nullifier_update(0xabcd, 15)])
        .expect("an update inside the window must be accepted");
    assert!(is_invalid_response(method.validate(&vec![nullifier_update(0xabcd, 25)])));
    assert!(is_invalid_response(method.validate(&vec![nullifier_update(0xabcd, 5)])));
}

#[test]
fn get_account_verifies_block_num_only_for_pinned_requests() {
    let response = (BlockNumber::from(5u32), account_proof());
    let method = |at| GetAccount {
        account_id: test_account_id(),
        request: GetAccountRequest::new().at(at),
    };

    method(AccountStateAt::ChainTip)
        .validate(&response)
        .expect("a chain tip request must accept state at any block");
    method(AccountStateAt::Block(BlockNumber::from(5u32)))
        .validate(&response)
        .expect("state at the requested block must be accepted");
    assert!(is_invalid_response(
        method(AccountStateAt::Block(BlockNumber::from(6u32))).validate(&response)
    ));
}

#[test]
fn get_account_rejects_proof_for_wrong_account_id() {
    // `account_proof()` is built for `test_account_id()`, but a different account is requested.
    let response = (BlockNumber::from(1u32), account_proof());
    let method = GetAccount {
        account_id: other_account_id(),
        request: GetAccountRequest::new(),
    };

    assert!(is_invalid_response(method.validate(&response)));
}

#[test]
fn get_note_script_by_root_verifies_script_root() {
    let script = StandardNote::P2ID.script();
    let method = GetNoteScriptByRoot { root: Word::from(script.root()) };

    method.validate(&None).expect("an unregistered root must pass through");
    method
        .validate(&Some(script))
        .expect("a script with the requested root must be accepted");
    assert!(is_invalid_response(method.validate(&Some(StandardNote::SWAP.script()))));
}

#[test]
fn sync_transactions_verifies_account_ids() {
    let requested = test_account_id();
    let other = other_account_id();
    let response = vec![transaction_record(requested)];
    let method = |account_ids| SyncTransactions {
        block_from: BlockNumber::GENESIS,
        block_to: BlockNumber::from(1u32),
        account_ids,
    };

    method(vec![requested])
        .validate(&response)
        .expect("the requested account must be accepted");
    method(vec![requested, other])
        .validate(&response)
        .expect("a superset of accounts must be accepted");
    assert!(is_invalid_response(method(vec![other]).validate(&response)));
    assert!(is_invalid_response(method(vec![]).validate(&response)));
}

#[test]
fn sync_transactions_accepts_empty_response() {
    let method = SyncTransactions {
        block_from: BlockNumber::GENESIS,
        block_to: BlockNumber::from(1u32),
        account_ids: vec![test_account_id()],
    };

    method.validate(&Vec::new()).expect("an empty response must be accepted");
}

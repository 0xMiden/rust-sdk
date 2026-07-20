use core::future::Future;
use core::pin::Pin;
use std::boxed::Box;
use std::collections::{BTreeMap, BTreeSet};
use std::env::temp_dir;
use std::println;
use std::sync::Arc;

use miden_client::ClientError;
use miden_client::account::{Address, AddressInterface};
use miden_client::address::RoutingParameters;
use miden_client::assembly::CodeBuilder;
use miden_client::auth::{
    AuthSchemeId,
    AuthSecretKey,
    AuthSingleSig,
    PublicKeyCommitment,
    RPO_FALCON_SCHEME_ID,
};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::note::{BlockNumber, NetworkAccountTarget, NoteExecutionHint};
use miden_client::pswap::PswapLineageState;
use miden_client::rpc::NodeRpcClient;
use miden_client::store::input_note_states::ConsumedAuthenticatedLocalNoteState;
use miden_client::store::{
    AccountStorageFilter,
    ClientAccountType,
    InputNoteRecord,
    InputNoteState,
    NoteFilter,
    OutputNoteState,
    TransactionFilter,
};
use miden_client::sync::{NoteTagRecord, NoteTagSource};
use miden_client::testing::common::{
    ACCOUNT_ID_REGULAR,
    MINT_AMOUNT,
    RECALL_HEIGHT_DELTA,
    TRANSFER_AMOUNT,
    TestClient,
    assert_account_has_single_asset,
    assert_note_cannot_be_consumed_twice,
    consume_notes,
    create_test_store_path,
    execute_failing_tx,
    mint_and_consume,
    mint_note,
    setup_two_wallets_and_faucet,
    setup_wallet_and_faucet,
};
use miden_client::testing::mock::{MockClient, MockRpcApi};
use miden_client::transaction::{
    DiscardCause,
    PaymentNoteDescription,
    PswapTransactionData,
    SwapTransactionData,
    TransactionExecutorError,
    TransactionRequestBuilder,
    TransactionRequestError,
    TransactionStatus,
};
use miden_client::utils::{Deserializable, Serializable};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::account::{
    Account,
    AccountBuilder,
    AccountCode,
    AccountComponent,
    AccountComponentMetadata,
    AccountHeader,
    AccountId,
    AccountIdVersion,
    AccountType,
    AssetCallbackFlag,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotContent,
    StorageSlotName,
};
use miden_protocol::asset::{Asset, AssetAmount, AssetId, FungibleAsset, TokenSymbol};
use miden_protocol::crypto::rand::{FeltRng, RandomCoin};
use miden_protocol::note::{
    Note,
    NoteAssets,
    NoteAttachments,
    NoteRecipient,
    NoteStorage,
    NoteTag,
    NoteType,
    PartialNoteMetadata,
};
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
    ACCOUNT_ID_PRIVATE_SENDER,
    ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1,
    ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2,
    ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET,
    ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE,
    ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
    ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE,
};
use miden_protocol::transaction::RawOutputNote;
use miden_protocol::vm::AdviceInputs;
use miden_protocol::{EMPTY_WORD, Felt, ONE, Word};
use miden_standards::account::AccountBuilderSchemaCommitmentExt;
use miden_standards::account::auth::Approver;
use miden_standards::account::faucets::{FungibleFaucet, TokenName};
use miden_standards::account::policies::{BurnPolicy, MintPolicy, TokenPolicyManager};
use miden_standards::account::wallets::BasicWallet;
use miden_standards::note::{
    NoteConsumptionStatus,
    NoteFile,
    NoteSyncHint,
    P2idNote,
    P2idNoteStorage,
    PswapNote,
    PswapNoteAttachment,
    StandardNote,
};
use miden_standards::testing::mock_account::MockAccountExt;
use miden_standards::testing::note::NoteBuilder;
use miden_standards::tx_script::SendNotesTransactionScriptError;
use miden_testing::{MockChain, MockChainBuilder, TxContextInput};
use rand::rngs::StdRng;
use rand::{Rng, RngExt, SeedableRng};
use rstest::rstest;

mod batch;
pub mod store;
mod transaction;
mod transport;

/// Constant that represents the number of blocks until the transaction is considered
/// stale.
const TX_DISCARD_DELTA: u32 = 20;

/// Number of storage map entries used to create accounts that exceed the oversize threshold.
const NUM_STORAGE_MAP_ENTRIES_LARGE_ACCOUNT: u64 = 2001;

/// Number of faucets (and therefore fungible assets) used in oversized-account tests.
const NUM_FAUCETS_LARGE_ACCOUNT: u64 = 10;

/// Oversize threshold used for the mock RPC in large-account tests.
/// Both storage map entries and vault assets must exceed this to trigger
/// the `too_many_entries` / `too_many_assets` flags.
const OVERSIZE_THRESHOLD: usize = 5;

// TESTS
// ================================================================================================

#[tokio::test]
async fn input_notes_round_trip() {
    // generate test client with a random store name
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;

    insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();
    // generate test data
    let available_notes = rpc_api.get_public_available_notes();

    // insert notes into database
    for note in &available_notes {
        client
            .import_notes(&[NoteFile::Committed {
                note: note.note().unwrap().clone(),
                proof: note.inclusion_proof().clone(),
            }])
            .await
            .unwrap();
    }

    // retrieve notes from database
    assert_eq!(client.get_input_notes(NoteFilter::Unverified).await.unwrap().len(), 1);
    // NOTE: the following asserts involve the spawn notes
    assert_eq!(client.get_input_notes(NoteFilter::Consumed).await.unwrap().len(), 3);
    let retrieved_notes = client.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(retrieved_notes.len(), 4);

    // Compare by details commitment, which is always available regardless of note state (a
    // `NoteId` needs metadata, which some records don't carry).
    let chain_notes_commitments: std::collections::HashSet<_> =
        available_notes.iter().map(|n| n.note().unwrap().details_commitment()).collect();
    // compare notes
    assert_eq!(
        chain_notes_commitments,
        retrieved_notes.iter().map(InputNoteRecord::details_commitment).collect()
    );
}

#[tokio::test]
async fn get_input_note() {
    // generate test client with a random store name
    let (mut client, rpc_api, _) = Box::pin(create_test_client()).await;
    // Get note from mocked RPC backend since any note works here
    let original_note = rpc_api.get_available_notes()[0].note().unwrap().clone();

    // insert Note into database
    let note: InputNoteRecord = original_note.clone().into();
    client
        .import_notes(&[NoteFile::ExpectedNote {
            details: note.clone().into(),
            sync_hint: NoteSyncHint::new(0.into(), note.metadata().unwrap().tag()),
        }])
        .await
        .unwrap();

    // The note is imported without metadata, so it's retrieved by its details commitment.
    let retrieved_note = client
        .get_input_notes(NoteFilter::DetailsCommitments(vec![original_note.details_commitment()]))
        .await
        .unwrap()
        .pop()
        .unwrap();

    let recorded_note: InputNoteRecord = original_note.into();
    assert_eq!(recorded_note.details_commitment(), retrieved_note.details_commitment());
}

type InsertAccountFuture<'client> =
    Pin<Box<dyn Future<Output = Result<Account, ClientError>> + 'client>>;

async fn assert_wallet_insertion<F>(insert_fn: F)
where
    F: for<'client> FnOnce(
        &'client mut TestClient,
        AccountType,
        &'client FilesystemKeyStore,
    ) -> InsertAccountFuture<'client>,
{
    let (mut client, _rpc_api, keystore) = Box::pin(create_test_client()).await;

    let account = insert_fn(&mut client, AccountType::Private, &keystore)
        .await
        .expect("account insertion should succeed");

    let account_reader = client.account_reader(account.id());

    // Verify account data via dedicated methods
    assert_eq!(account.nonce(), account_reader.nonce().await.unwrap());
    assert_eq!(account.vault().root(), account_reader.vault_root().await.unwrap());
    assert_eq!(account.code().commitment(), account_reader.code_commitment().await.unwrap());
    assert_eq!(
        account.storage().to_commitment(),
        account_reader.storage_commitment().await.unwrap()
    );

    // Verify seed
    let account_seed = account.seed();
    assert!(account_seed.is_some(), "newly built account should always contain a seed");
    assert_eq!(account_seed, account_reader.status().await.unwrap().seed().copied());
}

async fn assert_faucet_insertion<F>(insert_fn: F)
where
    F: for<'client> FnOnce(
        &'client mut TestClient,
        AccountType,
        &'client FilesystemKeyStore,
    ) -> InsertAccountFuture<'client>,
{
    let (mut client, _rpc_api, keystore) = Box::pin(create_test_client()).await;

    let account = insert_fn(&mut client, AccountType::Private, &keystore)
        .await
        .expect("account insertion should succeed");

    let account_reader = client.account_reader(account.id());

    // Verify account data via dedicated methods
    assert_eq!(account.nonce(), account_reader.nonce().await.unwrap());
    assert_eq!(account.vault().root(), account_reader.vault_root().await.unwrap());
    assert_eq!(account.code().commitment(), account_reader.code_commitment().await.unwrap());
    assert_eq!(
        account.storage().to_commitment(),
        account_reader.storage_commitment().await.unwrap()
    );

    // Verify seed
    let account_seed = account.seed();
    assert!(account_seed.is_some(), "newly built account should always contain a seed");
    assert_eq!(account_seed, account_reader.status().await.unwrap().seed().copied());
}

#[tokio::test]
async fn insert_basic_account() {
    assert_wallet_insertion(|client, visibility, keystore| {
        Box::pin(insert_new_wallet(client, visibility, keystore))
    })
    .await;
}

#[tokio::test]
async fn insert_ecdsa_account() {
    assert_wallet_insertion(|client, visibility, keystore| {
        Box::pin(insert_new_ecdsa_wallet(client, visibility, keystore))
    })
    .await;
}

#[tokio::test]
async fn insert_faucet_account() {
    assert_faucet_insertion(|client, visibility, keystore| {
        Box::pin(insert_new_fungible_faucet(client, visibility, keystore))
    })
    .await;
}

#[tokio::test]
async fn insert_ecdsa_faucet_account() {
    assert_faucet_insertion(|client, visibility, keystore| {
        Box::pin(insert_new_ecdsa_fungible_faucet(client, visibility, keystore))
    })
    .await;
}

#[tokio::test]
async fn insert_same_account_twice_fails() {
    // generate test client with a random store name
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )),
    );

    assert!(client.add_account(&account, false).await.is_ok());
    assert!(client.add_account(&account, false).await.is_err());
}

#[tokio::test]
async fn account_code() {
    // generate test client with a random store name
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock(
        ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )),
    );

    let account_code = account.code();

    let account_code_bytes = account_code.to_bytes();

    let reconstructed_code = AccountCode::read_from_bytes(&account_code_bytes).unwrap();
    assert_eq!(*account_code, reconstructed_code);

    client.add_account(&account, false).await.unwrap();
    let retrieved_code = client.get_account_code(account.id()).await.unwrap().unwrap();
    assert_eq!(*account.code(), retrieved_code);
}

#[tokio::test]
async fn get_account_by_id() {
    // generate test client with a random store name
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock(
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_UPDATABLE_CODE,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )),
    );

    client.add_account(&account, false).await.unwrap();

    // Retrieving an existing account should succeed
    let (acc_from_db, _account_seed) = match client.account_reader(account.id()).header().await {
        Ok(header_and_status) => header_and_status,
        Err(err) => panic!("Error retrieving account: {err}"),
    };
    assert_eq!(AccountHeader::from(account), acc_from_db);

    // Retrieving a non existing account should return error
    let invalid_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2).unwrap();
    assert!(client.account_reader(invalid_id).header().await.is_err());
}

#[tokio::test]
async fn sync_state() {
    // generate test client with a random store name
    let (mut client, rpc_api, _) = Box::pin(create_test_client()).await;

    // Import first mockchain note as expected
    let expected_notes = rpc_api
        .get_available_notes()
        .into_iter()
        .filter(|n| n.inclusion_proof().location().block_num() != BlockNumber::GENESIS)
        .map(|n| n.note().unwrap().clone())
        .collect::<Vec<Note>>();

    for note in &expected_notes {
        client
            .import_notes(&[NoteFile::ExpectedNote {
                details: note.clone().into(),
                sync_hint: NoteSyncHint::new(0.into(), note.metadata().tag()),
            }])
            .await
            .unwrap();
    }

    // assert that we have no consumed nor expected notes prior to syncing state
    assert_eq!(client.get_input_notes(NoteFilter::Consumed).await.unwrap().len(), 0);
    assert_eq!(
        client.get_input_notes(NoteFilter::Expected).await.unwrap().len(),
        expected_notes.len()
    );
    assert_eq!(client.get_input_notes(NoteFilter::Committed).await.unwrap().len(), 0);

    // sync state
    let sync_details = client.sync_state().await.unwrap();

    // verify that the client is synced to the latest block
    assert_eq!(sync_details.block_num, rpc_api.get_chain_tip_block_num());

    // verify that we now have one committed note after syncing state
    assert_eq!(client.get_input_notes(NoteFilter::Committed).await.unwrap().len(), 1);
    assert_eq!(client.get_input_notes(NoteFilter::Consumed).await.unwrap().len(), 1);
    assert_eq!(sync_details.consumed_notes.len(), 1);

    // verify that the latest block number has been updated
    assert_eq!(client.get_sync_height().await.unwrap(), rpc_api.get_chain_tip_block_num());
}

#[tokio::test]
async fn sync_state_mmr() {
    // generate test client with a random store name
    let (mut client, rpc_api, keystore) = Box::pin(create_test_client()).await;
    // Import note and create wallet so that synced notes do not get discarded (due to being
    // irrelevant)
    insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();

    // Import only public notes
    let notes = rpc_api
        .get_public_available_notes()
        .into_iter()
        .filter_map(|n| n.note().cloned())
        .collect::<Vec<Note>>();

    for note in &notes {
        client
            .import_notes(&[NoteFile::ExpectedNote {
                details: note.clone().into(),
                sync_hint: NoteSyncHint::new(0.into(), note.metadata().tag()),
            }])
            .await
            .unwrap();
    }

    // sync state
    let sync_details = client.sync_state().await.unwrap();

    // verify that the client is synced to the latest block
    assert_eq!(sync_details.block_num, rpc_api.get_chain_tip_block_num());

    // verify that the latest block number has been updated
    assert_eq!(client.get_sync_height().await.unwrap(), rpc_api.get_chain_tip_block_num());

    assert!(!client.test_has_cached_partial_mmr());

    // verify that we inserted the latest block into the DB via the client
    let latest_block = client.get_sync_height().await.unwrap();
    assert_eq!(sync_details.block_num, latest_block);
    assert_eq!(
        rpc_api.get_block_header_by_number(None, false).await.unwrap().0.commitment(),
        client
            .test_store()
            .get_block_headers(&[latest_block].into_iter().collect())
            .await
            .unwrap()[0]
            .0
            .commitment()
    );

    // Try reconstructing the partial_mmr from what's in the database
    let partial_mmr = client.test_store().get_current_partial_mmr().await.unwrap();
    assert!(partial_mmr.forest().num_leaves() >= 6);
    assert!(partial_mmr.open(0).unwrap().is_none());
    // Block 1 holds the only unspent public note, so its leaf stays tracked.
    assert!(partial_mmr.open(1).unwrap().is_some());
    assert!(partial_mmr.open(2).unwrap().is_none());
    assert!(partial_mmr.open(3).unwrap().is_none());
    // Block 4's notes are all consumed externally, so pruning untracks its leaf.
    assert!(partial_mmr.open(4).unwrap().is_none());
    assert!(partial_mmr.open(5).unwrap().is_none());

    // Ensure the proof for the remaining tracked leaf is valid
    let mmr_proof = partial_mmr.open(1).unwrap().unwrap();
    let (block_1, _) = rpc_api.get_block_header_by_number(Some(1.into()), false).await.unwrap();
    partial_mmr.peaks().verify(block_1.commitment(), mmr_proof).unwrap();

    // Only block 1 remains tracked after pruning; block 4 was untracked because all its
    // notes are already consumed externally.
    assert_eq!(client.test_store().get_tracked_block_headers().await.unwrap().len(), 1);
}

#[tokio::test]
async fn sync_state_mmr_with_in_memory_cache() {
    let (builder, rpc_api, keystore) = Box::pin(create_test_client_builder()).await;
    let mut client = builder.cache_partial_mmr_in_memory(true).build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();

    // First sync populates the cache.
    client.sync_state().await.unwrap();
    assert!(client.test_has_cached_partial_mmr());

    // Advance the chain and sync again to mutate the cached MMR.
    rpc_api.advance_blocks(2);
    client.sync_state().await.unwrap();

    // Cache must agree with the store.
    let cached = client.get_current_partial_mmr().await.unwrap();
    let stored = client.test_store().get_current_partial_mmr().await.unwrap();
    assert_eq!(cached.peaks(), stored.peaks());
    assert_eq!(cached.forest(), stored.forest());
}

/// Verifies the `get_current_partial_mmr` rebuild path: when the cache fingerprint diverges
/// from the store (here, by untracking a block directly via the store and bypassing
/// `cache_partial_mmr`), the next read must detect the divergence and return the rebuilt
/// store-backed MMR rather than the stale cache.
#[tokio::test]
async fn stale_cached_partial_mmr_is_rebuilt_from_store() {
    let (builder, rpc_api, keystore) = Box::pin(create_test_client_builder()).await;
    let mut client = builder.cache_partial_mmr_in_memory(true).build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();

    // Import the mock chain's public notes so a block becomes tracked after sync.
    let notes: Vec<Note> = rpc_api
        .get_public_available_notes()
        .into_iter()
        .filter_map(|n| n.note().cloned())
        .collect();
    for note in &notes {
        client
            .import_notes(&[NoteFile::ExpectedNote {
                details: note.clone().into(),
                sync_hint: NoteSyncHint::new(0.into(), note.metadata().tag()),
            }])
            .await
            .unwrap();
    }
    client.sync_state().await.unwrap();
    assert!(client.test_has_cached_partial_mmr());

    // Pick any tracked block. The mock chain has an unspent public note in block 1,
    // so the tracked set is non-empty after the sync above.
    let tracked: Vec<usize> = client
        .test_store()
        .get_tracked_block_header_numbers()
        .await
        .unwrap()
        .into_iter()
        .collect();
    let to_untrack = BlockNumber::from(u32::try_from(tracked[0]).unwrap());

    // Confirm the cache currently sees the leaf as tracked.
    let cached_before = client.get_current_partial_mmr().await.unwrap();
    assert!(cached_before.open(to_untrack.as_usize()).unwrap().is_some());

    // Mutate the store directly to untrack the block. This bypasses `cache_partial_mmr`,
    // so the cached fingerprint stays stale.
    client
        .test_store()
        .untrack_and_prune_irrelevant_blocks(&[to_untrack], &[])
        .await
        .unwrap();

    // The freshness check must detect the tracked-set divergence and rebuild from the
    // store. A blind cache hit would still report the leaf as tracked.
    let after = client.get_current_partial_mmr().await.unwrap();
    let stored = client.test_store().get_current_partial_mmr().await.unwrap();

    assert_eq!(after.peaks(), stored.peaks());
    assert_eq!(after.forest(), stored.forest());
    assert!(
        after.open(to_untrack.as_usize()).unwrap().is_none(),
        "stale cache returned: rebuild path did not fire",
    );
}

/// Tests that MMR authentication nodes are persisted even when `include_block` is false
/// (i.e., a synced block has no relevant notes and is not the chain tip).
///
/// This covers the scenario where a browser extension popup is closed and reopened:
/// the in-memory `PartialMmr` is lost and must be fully reconstructable from the store.
/// Without persisting auth nodes for skipped blocks, the store would be missing nodes
/// needed for Merkle authentication paths, causing transaction execution to fail.
#[tokio::test]
async fn sync_persists_auth_nodes_for_skipped_blocks() {
    use miden_client::async_trait;
    use miden_client::rpc::domain::note::CommittedNote;
    use miden_client::store::InputNoteRecord;
    use miden_client::sync::{NoteUpdateAction, OnNoteReceived, StateSync, StateSyncInput};
    use miden_protocol::crypto::merkle::mmr::{Forest, MmrPeaks, PartialMmr};

    // A note screener that discards all notes, forcing `found_relevant_note = false`
    // for every sync step. This means only the chain tip will have `include_block = true`.
    struct DiscardAllNotes;

    #[async_trait(?Send)]
    impl OnNoteReceived for DiscardAllNotes {
        async fn on_note_received(
            &self,
            _committed_note: CommittedNote,
            _public_note: Option<InputNoteRecord>,
        ) -> Result<NoteUpdateAction, ClientError> {
            Ok(NoteUpdateAction::Discard)
        }
    }

    // Set up the mock chain (blocks 0-5, notes in blocks 1 and 4)
    let (_client, rpc_api, _) = Box::pin(create_test_client()).await;

    // Build a PartialMmr starting from an empty forest with the genesis block tracked.
    // Tracking genesis is critical: it means the MMR must produce authentication nodes
    // for genesis whenever the tree structure changes (i.e., when new blocks are added).
    let genesis = rpc_api.get_block_header_by_number(Some(0.into()), false).await.unwrap().0;
    let mut partial_mmr = PartialMmr::from_peaks(MmrPeaks::new(Forest::empty(), vec![]).unwrap());
    partial_mmr.add(genesis.commitment(), true).unwrap(); // track genesis

    // Create a StateSync that discards all notes so intermediate blocks are skipped
    let state_sync = StateSync::new(Arc::new(rpc_api.clone()), Arc::new(DiscardAllNotes), None);

    // Use the note tag from the prebuilt chain (tag 0) so the mock RPC returns
    // blocks step-by-step (block 1, then block 4, then the chain tip) instead of
    // jumping directly to the chain tip.
    let note_tags = BTreeSet::from([NoteTag::new(0)]);

    let state_sync_update = state_sync
        .sync_state(
            &mut partial_mmr,
            StateSyncInput {
                accounts: vec![],
                note_tags,
                input_notes: vec![],
                output_notes: vec![],
                uncommitted_transactions: vec![],
            },
        )
        .await
        .unwrap();

    // Only the chain tip block should be stored as a block header.
    // Blocks 1 and 4 had matching note tags but the screener discarded them,
    // so `include_block` was false for those steps.
    assert_eq!(
        state_sync_update.partial_blockchain_updates.block_headers().count(),
        1,
        "expected only the chain tip block header to be stored"
    );
    let (tip_header, ..) =
        state_sync_update.partial_blockchain_updates.block_headers().next().unwrap();
    assert_eq!(tip_header.block_num(), rpc_api.get_chain_tip_block_num());

    // Authentication nodes must be non-empty: they include nodes produced by applying
    // the MMR delta and adding the chain tip leaf. These nodes are needed for the
    // tracked genesis leaf's Merkle proof path, which changes as the tree grows.
    assert!(
        !state_sync_update
            .partial_blockchain_updates
            .new_authentication_nodes()
            .is_empty(),
        "expected authentication nodes from intermediate (skipped) blocks to be persisted"
    );
}

/// Tests that a public account modified across multiple sync steps only triggers a single
/// `/GetAccount` RPC call, not one per sync step.
#[tokio::test]
async fn sync_state_no_redundant_get_account_calls() {
    use miden_client::async_trait;
    use miden_client::rpc::domain::note::CommittedNote;
    use miden_client::store::InputNoteRecord;
    use miden_client::sync::{NoteUpdateAction, OnNoteReceived, StateSync, StateSyncInput};
    use miden_protocol::crypto::merkle::mmr::{Forest, MmrPeaks, PartialMmr};

    struct DiscardAllNotes;

    #[async_trait(?Send)]
    impl OnNoteReceived for DiscardAllNotes {
        async fn on_note_received(
            &self,
            _committed_note: CommittedNote,
            _public_note: Option<InputNoteRecord>,
        ) -> Result<NoteUpdateAction, ClientError> {
            Ok(NoteUpdateAction::Discard)
        }
    }

    // Set up the mock chain (blocks 0-5, account modified in blocks 1, 4, 5)
    let (_client, rpc_api, _) = Box::pin(create_test_client()).await;

    // Find the public account ID from the mock chain's proven blocks
    let account_id = {
        let mock_chain = rpc_api.mock_chain.read();
        mock_chain
            .proven_blocks()
            .iter()
            .flat_map(|b| b.body().updated_accounts().iter())
            .map(miden_protocol::block::BlockAccountUpdate::account_id)
            .find(|id| !id.is_private())
            .expect("prebuilt mock chain should have a public account")
    };

    // Create an AccountHeader with stale state (nonce 0, dummy commitments).
    // This ensures every sync step's reported commitment differs from our local header,
    // which would trigger a fetch in every step without the fix.
    let account_header =
        AccountHeader::new(account_id, Felt::from(0u32), EMPTY_WORD, EMPTY_WORD, EMPTY_WORD);

    // Build a PartialMmr starting from genesis
    let genesis = rpc_api.get_block_header_by_number(Some(0.into()), false).await.unwrap().0;
    let mut partial_mmr = PartialMmr::from_peaks(MmrPeaks::new(Forest::empty(), vec![]).unwrap());
    partial_mmr.add(genesis.commitment(), true).unwrap();

    let state_sync = StateSync::new(Arc::new(rpc_api.clone()), Arc::new(DiscardAllNotes), None);

    // Use tag 0 to force multiple sync steps (notes exist in blocks 1 and 4)
    let note_tags = BTreeSet::from([NoteTag::new(0)]);

    let input = StateSyncInput {
        accounts: vec![account_header],
        note_tags,
        input_notes: vec![],
        output_notes: vec![],
        uncommitted_transactions: vec![],
    };
    let state_sync_update = state_sync.sync_state(&mut partial_mmr, input).await.unwrap();

    // Only 1 updated public account entry, not N duplicates
    assert_eq!(
        state_sync_update.account_updates.updated_public_accounts().len(),
        1,
        "expected exactly 1 updated public account"
    );
}

#[tokio::test]
async fn sync_state_tags() {
    // generate test client with a random store name
    let (mut client, rpc_api, _) = Box::pin(create_test_client()).await;

    // Import first mockchain note as expected
    let expected_notes = rpc_api.get_available_notes();
    for tag in expected_notes.iter().map(|n| n.metadata().tag()) {
        client.add_note_tag(tag).await.unwrap();
    }

    // assert that we have no expected notes prior to syncing state
    assert!(client.get_input_notes(NoteFilter::Expected).await.unwrap().is_empty());

    // sync state
    // The mockchain API has one public note and one private note, so in the end we will have
    // the public one in the client
    let sync_details = client.sync_state().await.unwrap();

    // verify that the client is synced to the latest block
    assert_eq!(
        sync_details.block_num,
        rpc_api.get_block_header_by_number(None, false).await.unwrap().0.block_num()
    );

    // as we are syncing with tags, the response should contain blocks for both notes
    assert_eq!(client.get_input_notes(NoteFilter::All).await.unwrap().len(), 2);
    // Only the public note is unspent; the private note is consumed externally, so its
    // block is pruned immediately after sync.
    assert_eq!(client.test_store().get_tracked_block_headers().await.unwrap().len(), 1);
}

#[tokio::test]
async fn tags() {
    // generate test client with a random store name
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    // Assert that the store gets created with the tag 0 (used for notes consumable by any account)
    assert!(client.get_note_tags().await.unwrap().is_empty());

    // add a tag
    let tag_1: NoteTag = 1.into();
    let tag_2: NoteTag = 2.into();
    client.add_note_tag(tag_1).await.unwrap();
    client.add_note_tag(tag_2).await.unwrap();

    // verify that the tag is being tracked
    assert_eq!(client.get_note_tags().await.unwrap(), vec![tag_1, tag_2]);

    // attempt to add the same tag again
    client.add_note_tag(tag_1).await.unwrap();

    // verify that the tag is still being tracked only once
    assert_eq!(client.get_note_tags().await.unwrap(), vec![tag_1, tag_2]);

    // Try removing non-existent tag
    let tag_4: NoteTag = 4.into();
    client.remove_note_tag(tag_4).await.unwrap();

    // verify that the tracked tags are unchanged
    assert_eq!(client.get_note_tags().await.unwrap(), vec![tag_1, tag_2]);

    // remove second tag
    client.remove_note_tag(tag_1).await.unwrap();

    // verify that tag_1 is not tracked anymore
    assert_eq!(client.get_note_tags().await.unwrap(), vec![tag_2]);
}

#[tokio::test]
async fn mint_transaction() {
    // generate test client with a random store name
    let (mut client, _rpc_api, keystore) = Box::pin(create_test_client()).await;

    // Faucet account generation
    let faucet = insert_new_fungible_faucet(&mut client, AccountType::Private, &keystore)
        .await
        .unwrap();

    client.sync_state().await.unwrap();

    // Test submitting a mint transaction
    let transaction_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1).unwrap(),
            miden_protocol::note::NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let transaction_result =
        Box::pin(client.execute_transaction(faucet.id(), transaction_request.clone()))
            .await
            .unwrap();
    let executed_tx = transaction_result.executed_transaction().clone();

    assert_eq!(executed_tx.account_patch().final_nonce(), Some(ONE));
}

#[tokio::test]
async fn import_note_validation() {
    // generate test client
    let (mut client, rpc_api, _) = Box::pin(create_test_client()).await;

    // generate deterministic test data
    let available_notes = rpc_api.get_available_notes();
    let mut expected_note = None;
    let mut consumed_note = None;

    for note in &available_notes {
        let Some(public_note) = note.note() else { continue };
        let nullifiers = rpc_api
            .get_nullifier_commit_heights(
                BTreeSet::from([public_note.nullifier()]),
                note.inclusion_proof().location().block_num(),
            )
            .await
            .unwrap();

        let nullifier_consumed = nullifiers.get(&public_note.nullifier()).unwrap();
        if nullifier_consumed.is_some() {
            consumed_note = Some(note.clone());
        } else if expected_note.is_none() {
            expected_note = Some(note.clone());
        }

        if consumed_note.is_some() && expected_note.is_some() {
            break;
        }
    }

    let expected_note = expected_note.expect("expected to find at least one unconsumed note");
    let consumed_note = consumed_note.expect("expected to find at least one consumed note");

    client
        .import_notes(&[NoteFile::Committed {
            note: consumed_note.note().unwrap().clone(),
            proof: consumed_note.inclusion_proof().clone(),
        }])
        .await
        .unwrap();

    client
        .import_notes(&[NoteFile::ExpectedNote {
            details: expected_note.note().unwrap().into(),
            sync_hint: NoteSyncHint::new(0.into(), expected_note.note().unwrap().metadata().tag()),
        }])
        .await
        .unwrap();

    // The expected note was imported without metadata, so it's retrieved by its details commitment.
    let expected_note = Box::pin(client.get_input_notes(NoteFilter::DetailsCommitments(vec![
        expected_note.note().unwrap().details_commitment(),
    ])))
    .await
    .unwrap()
    .pop()
    .unwrap();

    // Retrieve the consumed note by its details commitment, which is stable across state
    // transitions.
    let consumed_note = client
        .get_input_notes(NoteFilter::DetailsCommitments(vec![
            consumed_note.note().unwrap().details_commitment(),
        ]))
        .await
        .unwrap()
        .pop()
        .unwrap();

    assert!(expected_note.inclusion_proof().is_none());
    assert!(consumed_note.is_consumed());
}

#[tokio::test]
async fn transaction_request_expiration() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    client.sync_state().await.unwrap();

    let current_height = client.get_sync_height().await.unwrap();
    let faucet = insert_new_fungible_faucet(&mut client, AccountType::Private, &keystore)
        .await
        .unwrap();

    let transaction_request = TransactionRequestBuilder::new()
        .expiration_delta(5)
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap(),
            miden_protocol::note::NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let transaction_result =
        Box::pin(client.execute_transaction(faucet.id(), transaction_request.clone()))
            .await
            .unwrap();

    let (_, tx_outputs, ..) = transaction_result.executed_transaction().clone().into_parts();

    assert_eq!(tx_outputs.expiration_block_num(), current_height + 5);
}

#[tokio::test]
async fn import_processing_note_returns_error() {
    // generate test client with a random store name
    let (mut client, _rpc_api, keystore) = Box::pin(create_test_client()).await;
    client.sync_state().await.unwrap();

    let account = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();

    // Faucet account generation
    let faucet = insert_new_fungible_faucet(&mut client, AccountType::Private, &keystore)
        .await
        .unwrap();

    // Test submitting a mint transaction
    let transaction_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            account.id(),
            miden_protocol::note::NoteType::Public,
            client.rng(),
        )
        .unwrap();

    Box::pin(client.submit_new_transaction(faucet.id(), transaction_request.clone()))
        .await
        .unwrap();

    let note_id = transaction_request.expected_output_own_notes().pop().unwrap().id();
    let note = client.get_input_note(note_id).await.unwrap().unwrap();

    let input = [(note.try_into().unwrap(), None)];
    let consume_note_request = TransactionRequestBuilder::new().input_notes(input).build().unwrap();
    Box::pin(client.submit_new_transaction(account.id(), consume_note_request))
        .await
        .unwrap();

    let processing_notes = client.get_input_notes(NoteFilter::Processing).await.unwrap();

    assert!(matches!(
        client
            .import_notes(&[NoteFile::NoteId(
                processing_notes[0].id().expect("processing note has metadata so id() is Some")
            )])
            .await
            .unwrap_err(),
        ClientError::NoteImportError { .. }
    ));
}

// TODO: fix - blocked by an upstream miden-standards bug (0.16.0-alpha.2). The
// `send_notes_script.rs::move_asset_to_note_body` helper only emits the `pad(21)->pad(16)` stack
// reduction inside the per-asset loop, so a zero-asset output note created from a basic wallet
// returns at stack depth 21 and the VM rejects the transaction with `InvalidStackDepthOnReturn`.
// Re-enable once the standards send-notes script handles zero-asset notes.
#[tokio::test]
async fn note_without_asset() {
    let (mut client, _rpc_api, keystore) = Box::pin(create_test_client()).await;

    let faucet = insert_new_fungible_faucet(&mut client, AccountType::Private, &keystore)
        .await
        .unwrap();

    let wallet = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();

    client.sync_state().await.unwrap();

    // Create note without assets
    let serial_num = client.rng().draw_word();
    let recipient = P2idNoteStorage::new(wallet.id()).into_recipient(serial_num);
    let tag = NoteTag::with_account_target(wallet.id());
    let metadata = PartialNoteMetadata::new(wallet.id(), NoteType::Private).with_tag(tag);
    let vault = NoteAssets::new(vec![]).unwrap();

    let note = Note::new(vault.clone(), metadata, recipient.clone());

    // Create and execute transaction
    let transaction_request =
        TransactionRequestBuilder::new().own_output_notes(vec![note]).build().unwrap();

    let transaction =
        Box::pin(client.execute_transaction(wallet.id(), transaction_request.clone())).await;

    assert!(transaction.is_ok());

    // Create the same transaction for the faucet
    let metadata = PartialNoteMetadata::new(faucet.id(), NoteType::Private).with_tag(tag);
    let note = Note::new(vault, metadata, recipient);

    let transaction_request =
        TransactionRequestBuilder::new().own_output_notes(vec![note]).build().unwrap();

    let error = Box::pin(client.submit_new_transaction(faucet.id(), transaction_request))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ClientError::TransactionRequestError(
            TransactionRequestError::SendNotesTransactionScriptError(
                SendNotesTransactionScriptError::FaucetNoteWithoutAsset
            )
        )
    ));

    let error = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![], faucet.id(), wallet.id()),
            NoteType::Public,
            client.rng(),
        )
        .unwrap_err();

    assert!(matches!(error, TransactionRequestError::P2IDNoteWithoutAsset));

    let error = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(FungibleAsset::new(faucet.id(), 0).unwrap())],
                faucet.id(),
                wallet.id(),
            ),
            NoteType::Public,
            client.rng(),
        )
        .unwrap_err();

    assert!(matches!(error, TransactionRequestError::P2IDNoteWithoutAsset));
}

#[tokio::test]
async fn execute_program() {
    let (mut client, _, keystore) = Box::pin(create_test_client()).await;
    let _ = client.sync_state().await.unwrap();

    let wallet = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();

    let code = "
        use miden::core::sys

        @transaction_script
        pub proc main
            push.16
            repeat.16
                dup push.1 sub
            end
            exec.sys::truncate_stack
        end
        ";

    let tx_script = client.code_builder().compile_tx_script(code).unwrap();

    let output_stack = Box::pin(client.execute_program(
        wallet.id(),
        tx_script,
        AdviceInputs::default(),
        BTreeMap::new(),
    ))
    .await
    .unwrap();

    let mut expected_stack = [Felt::from(0u32); 16];
    for (i, element) in expected_stack.iter_mut().enumerate() {
        *element = Felt::new_unchecked(i as u64);
    }

    assert_eq!(output_stack, expected_stack);
}

#[tokio::test]
async fn real_note_roundtrip() {
    let (mut client, mock_rpc_api, keystore) = Box::pin(create_test_client()).await;
    let wallet = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();
    let faucet = insert_new_fungible_faucet(&mut client, AccountType::Private, &keystore)
        .await
        .unwrap();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Test submitting a mint transaction
    let transaction_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            wallet.id(),
            miden_protocol::note::NoteType::Public,
            client.rng(),
        )
        .unwrap();

    let note_id = transaction_request.expected_output_own_notes().pop().unwrap().id();
    Box::pin(client.submit_new_transaction(faucet.id(), transaction_request))
        .await
        .unwrap();

    let note = client.get_input_note(note_id).await.unwrap().unwrap();
    assert!(matches!(note.state(), &InputNoteState::Expected(_)));

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note = client.get_input_note(note_id).await.unwrap().unwrap();
    assert!(matches!(note.state(), &InputNoteState::Committed(_)));

    // Consume note
    let transaction_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone().try_into().unwrap()])
        .unwrap();

    Box::pin(client.submit_new_transaction(wallet.id(), transaction_request))
        .await
        .unwrap();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let note = client.get_input_note(note_id).await.unwrap().unwrap();
    assert!(matches!(note.state(), &InputNoteState::ConsumedAuthenticatedLocal(_)));
}

#[tokio::test]
async fn added_notes() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let faucet_account_header =
        insert_new_fungible_faucet(&mut client, AccountType::Private, &authenticator)
            .await
            .unwrap();

    // Mint some asset for an account not tracked by the client. It should not be stored as an
    // input note afterwards since it is not being tracked by the client
    let fungible_asset = FungibleAsset::new(faucet_account_header.id(), MINT_AMOUNT).unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            fungible_asset,
            AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap(),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    println!("Running Mint tx...");
    Box::pin(client.submit_new_transaction(faucet_account_header.id(), tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that no new notes were added
    println!("Fetching Committed Notes...");
    let notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(notes.is_empty());
}

#[tokio::test]
async fn p2id_transfer() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First Mint necessary token
    mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    assert_account_has_single_asset(&client, from_account_id, faucet_account_id, MINT_AMOUNT).await;

    // Do a transfer from first account to second account
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    println!("Running P2ID tx...");
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let note = tx_request.expected_output_own_notes().pop().unwrap();
    Box::pin(client.submit_new_transaction(from_account_id, tx_request))
        .await
        .unwrap();

    // Check that a note tag started being tracked for this note.
    assert!(
        client
            .get_note_tags()
            .await
            .unwrap()
            .into_iter()
            .any(|tag| tag.source == NoteTagSource::Note(note.details_commitment()))
    );

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that the tag is not longer being tracked
    assert!(
        !client
            .get_note_tags()
            .await
            .unwrap()
            .into_iter()
            .any(|tag| tag.source == NoteTagSource::Note(note.details_commitment()))
    );

    // Check that note is committed for the second account to consume
    println!("Fetching Committed Notes...");
    let notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(!notes.is_empty());

    // Consume P2ID note
    println!("Consuming Note...");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![notes[0].clone().try_into().unwrap()])
        .unwrap();
    Box::pin(client.submit_new_transaction(to_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Ensure we have nothing else to consume
    let current_notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(current_notes.is_empty());

    let status = client.account_reader(from_account_id).status().await.unwrap();

    // The seed should not be retrieved due to the account not being new
    assert!(!status.is_new() && status.seed().is_none());

    // Validate the transferred amounts
    let from_balance = client
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(from_balance, AssetAmount::new(MINT_AMOUNT - TRANSFER_AMOUNT).unwrap());

    let to_balance = client
        .account_reader(to_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(to_balance, AssetAmount::new(TRANSFER_AMOUNT).unwrap());

    assert_note_cannot_be_consumed_twice(
        &mut client,
        to_account_id,
        notes[0].clone().try_into().unwrap(),
    )
    .await;
}

#[tokio::test]
async fn input_note_reader_finds_externally_consumed_notes() {
    let sender_id: AccountId = ACCOUNT_ID_PRIVATE_SENDER.try_into().unwrap();
    let faucet_id: AccountId = ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET.try_into().unwrap();

    // Build a MockChain with a sender, consumer, and a P2ID note from sender to consumer.
    let mut builder = MockChainBuilder::new();
    let consumer = builder.add_existing_mock_account(miden_testing::Auth::IncrNonce).unwrap();
    let consumer_id = consumer.id();

    let asset = Asset::Fungible(FungibleAsset::new(faucet_id, 100u64).unwrap());
    let p2id_note = builder
        .add_p2id_note(sender_id, consumer_id, &[asset], NoteType::Public)
        .unwrap();
    let p2id_details_commitment = p2id_note.details_commitment();
    let p2id_tag = p2id_note.metadata().tag();

    let mut chain = builder.build().unwrap();
    // Block 1: makes the note consumable.
    chain.prove_next_block().unwrap();

    // Consumer consumes the note directly on the chain (bypassing any client).
    let tx = Box::pin(
        chain
            .build_tx_context(
                miden_testing::TxContextInput::Account(consumer.clone()),
                &[],
                core::slice::from_ref(&p2id_note),
            )
            .unwrap()
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    chain.add_pending_executed_transaction(&tx).unwrap();
    // Block 2: includes the consume transaction.
    chain.prove_next_block().unwrap();

    // Build a client backed by this chain.
    let rng =
        RandomCoin::new(rand::random::<[u64; 4]>().map(|v| Felt::new_unchecked(v >> 1)).into());
    let keystore_path = std::env::temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path).unwrap();
    let mock_rpc = MockRpcApi::new(chain);

    let mut client = ClientBuilder::new()
        .rpc(Arc::new(mock_rpc))
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    // Register the consumer account so sync_transactions returns its transactions.
    client.add_account(&consumer, false).await.unwrap();

    // Import the P2ID note as an input note so the client tracks it. The tag lets the import
    // resolve the note's on-chain commitment (and thus its metadata) so it can later be matched
    // when the external consumer's nullifier appears during sync.
    let note_file = NoteFile::ExpectedNote {
        details: p2id_note.into(),
        sync_hint: NoteSyncHint::new(BlockNumber::from(0u32), p2id_tag),
    };
    client.import_notes(&[note_file]).await.unwrap();

    // Sync: the client should discover the note was consumed externally by the tracked account.
    client.sync_state().await.unwrap();

    // Retrieve the note by its details commitment, which is stable across state transitions.
    let input_note = client
        .get_input_notes(NoteFilter::DetailsCommitments(vec![p2id_details_commitment]))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        matches!(input_note.state(), InputNoteState::ConsumedExternal(..)),
        "Note should be in ConsumedExternal state, got: {}",
        input_note.state(),
    );
    assert_eq!(
        input_note.consumer_account(),
        Some(consumer_id),
        "consumer_account should be set to the tracked account that consumed the note",
    );

    // InputNoteReader should surface the externally-consumed note.
    let mut reader = client.input_note_reader(consumer_id);
    let mut collected = Vec::new();
    while let Some(n) = reader.next().await.unwrap() {
        collected.push(n);
    }

    assert_eq!(
        collected.len(),
        1,
        "InputNoteReader should return the externally-consumed note for the tracked consumer",
    );
    assert_eq!(collected[0].details_commitment(), p2id_details_commitment);
    assert_eq!(collected[0].consumer_account(), Some(consumer_id));
}

// Regression: importing an already-consumed public note by id must leave a record that is findable
// by its `NoteId` (not only by details commitment). The consumed record retains its metadata, so it
// keeps a resolvable id.
#[tokio::test]
async fn import_by_id_already_consumed_note_is_findable_by_id() {
    let sender_id: AccountId = ACCOUNT_ID_PRIVATE_SENDER.try_into().unwrap();
    let faucet_id: AccountId = ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET.try_into().unwrap();

    // Build a MockChain with a consumer and a PUBLIC P2ID note from sender to consumer.
    let mut builder = MockChainBuilder::new();
    let consumer = builder.add_existing_mock_account(miden_testing::Auth::IncrNonce).unwrap();
    let consumer_id = consumer.id();

    let asset = Asset::Fungible(FungibleAsset::new(faucet_id, 100u64).unwrap());
    let p2id_note = builder
        .add_p2id_note(sender_id, consumer_id, &[asset], NoteType::Public)
        .unwrap();
    let note_id = p2id_note.id();
    let details_commitment = p2id_note.details_commitment();

    let mut chain = builder.build().unwrap();
    // Block 1: makes the note consumable.
    chain.prove_next_block().unwrap();

    // Consumer consumes the note directly on the chain (bypassing any client).
    let tx = Box::pin(
        chain
            .build_tx_context(
                miden_testing::TxContextInput::Account(consumer.clone()),
                &[],
                core::slice::from_ref(&p2id_note),
            )
            .unwrap()
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    chain.add_pending_executed_transaction(&tx).unwrap();
    // Block 2: includes the consume transaction (note is now spent on-chain).
    chain.prove_next_block().unwrap();

    // Build a client backed by this chain. This client never saw the note before.
    let rng =
        RandomCoin::new(rand::random::<[u64; 4]>().map(|v| Felt::new_unchecked(v >> 1)).into());
    let keystore = FilesystemKeyStore::new(std::env::temp_dir()).unwrap();
    let mock_rpc = MockRpcApi::new(chain);

    let mut client = ClientBuilder::new()
        .rpc(Arc::new(mock_rpc))
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    // Import the already-consumed public note by id.
    let returned = client.import_notes(&[NoteFile::NoteId(note_id)]).await.unwrap();
    assert_eq!(returned.len(), 1, "import returns the note's details commitment");

    // The consumed record keeps its NoteId, so it is findable by an id-based filter.
    let by_id = client.get_input_notes(NoteFilter::List(vec![note_id])).await.unwrap();
    assert_eq!(by_id.len(), 1, "consumed note must be findable by NoteId");
    assert_eq!(by_id[0].id(), Some(note_id));
    assert!(by_id[0].is_consumed());

    // It remains findable by details commitment too.
    let by_commitment = client
        .get_input_notes(NoteFilter::DetailsCommitments(vec![details_commitment]))
        .await
        .unwrap();
    assert_eq!(by_commitment.len(), 1);
}

/// Builds a chain with two blocks relevant to a tracked account (blocks 1 and 4) and a client
/// synced up to block 4 with `prune_interval` configured. Returns the consuming pieces needed to
/// make block 4 irrelevant on demand.
async fn setup_prunable_block_scenario(
    prune_interval: Option<u32>,
) -> (MockClient<FilesystemKeyStore>, MockRpcApi, AccountId, Note) {
    let mut builder = MockChainBuilder::new();
    let mock_account = builder.add_existing_mock_account(miden_testing::Auth::IncrNonce).unwrap();

    let note_first = NoteBuilder::new(
        mock_account.id(),
        RandomCoin::new([0, 0, 0, 0].map(Felt::new_unchecked).into()),
    )
    .note_type(NoteType::Public)
    .tag(NoteTag::new(0).into())
    .build()
    .unwrap();
    let note_second = NoteBuilder::new(
        mock_account.id(),
        RandomCoin::new([0, 0, 0, 1].map(Felt::new_unchecked).into()),
    )
    .note_type(NoteType::Public)
    .tag(NoteTag::new(0).into())
    .build()
    .unwrap();

    let spawn_note_1 = builder.add_spawn_note(std::slice::from_ref(&note_first)).unwrap();
    let spawn_note_2 = builder.add_spawn_note(std::slice::from_ref(&note_second)).unwrap();

    let mut chain = builder.build().unwrap();

    // Block 1: create the first unspent note (keeps block 1 permanently relevant).
    let tx = Box::pin(
        chain
            .build_tx_context(TxContextInput::AccountId(mock_account.id()), &[], &[spawn_note_1])
            .unwrap()
            .extend_expected_output_notes(vec![RawOutputNote::Full(note_first)])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    chain.add_pending_executed_transaction(&tx).unwrap();
    chain.prove_next_block().unwrap();

    // Blocks 2 and 3: advance the chain without changing tracked notes.
    chain.prove_next_block().unwrap();
    chain.prove_next_block().unwrap();

    // Block 4: create the second note, which the caller will later consume to make block 4
    // irrelevant.
    let tx = Box::pin(
        chain
            .build_tx_context(TxContextInput::AccountId(mock_account.id()), &[], &[spawn_note_2])
            .unwrap()
            .extend_expected_output_notes(vec![RawOutputNote::Full(note_second.clone())])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    chain.add_pending_executed_transaction(&tx).unwrap();
    chain.prove_next_block().unwrap();

    let rng =
        RandomCoin::new(rand::random::<[u64; 4]>().map(|v| Felt::new_unchecked(v >> 1)).into());
    let keystore = FilesystemKeyStore::new(std::env::temp_dir()).unwrap();
    let mock_rpc = MockRpcApi::new(chain);

    let mut client = ClientBuilder::new()
        .rpc(Arc::new(mock_rpc.clone()))
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .irrelevant_block_prune_interval(prune_interval)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    client.add_note_tag(NoteTag::new(0)).await.unwrap();

    client.sync_state().await.unwrap();
    assert_eq!(
        client.test_store().get_tracked_block_headers().await.unwrap().len(),
        2,
        "setup precondition: two relevant blocks tracked",
    );

    (client, mock_rpc, mock_account.id(), note_second)
}

/// Consumes `note` against `account_id` on the mocked chain and proves the resulting block, so the
/// block that originally carried the note becomes irrelevant from the client's perspective.
async fn consume_note_and_prove(mock_rpc: &MockRpcApi, account_id: AccountId, note: Note) {
    let tx = {
        let tx_context = mock_rpc
            .mock_chain
            .write()
            .build_tx_context(TxContextInput::AccountId(account_id), &[], &[note])
            .unwrap()
            .build()
            .unwrap();
        Box::pin(tx_context.execute()).await.unwrap()
    };
    mock_rpc.mock_chain.write().add_pending_executed_transaction(&tx).unwrap();
    mock_rpc.prove_block();
}

#[tokio::test]
async fn irrelevant_block_pruning_respects_sync_interval() {
    let (mut client, mock_rpc, account_id, note_second) =
        setup_prunable_block_scenario(Some(2)).await;

    consume_note_and_prove(&mock_rpc, account_id, note_second).await;

    client.sync_state().await.unwrap();
    assert_eq!(
        client.test_store().get_tracked_block_headers().await.unwrap().len(),
        2,
        "pruning should be deferred until the configured sync interval elapses",
    );

    mock_rpc.prove_block();
    client.sync_state().await.unwrap();
    assert_eq!(
        client.test_store().get_tracked_block_headers().await.unwrap().len(),
        1,
        "the irrelevant block should be pruned once the sync interval is reached",
    );
}

#[tokio::test]
async fn irrelevant_block_pruning_disabled_when_interval_is_none() {
    let (mut client, mock_rpc, account_id, note_second) = setup_prunable_block_scenario(None).await;

    consume_note_and_prove(&mock_rpc, account_id, note_second).await;

    // With pruning disabled, both tracked blocks must remain across repeated syncs.
    for _ in 0..5 {
        mock_rpc.prove_block();
        client.sync_state().await.unwrap();
        assert_eq!(
            client.test_store().get_tracked_block_headers().await.unwrap().len(),
            2,
            "no pruning should occur when the prune interval is None",
        );
    }
}

#[tokio::test]
async fn p2id_transfer_failing_not_enough_balance() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First Mint necessary token
    mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Do a transfer from first account to second account
    let asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT + 1).unwrap();
    println!("Running P2ID tx...");
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    execute_failing_tx(
        &mut client,
        from_account_id,
        tx_request,
        ClientError::AssetError(
            miden_protocol::errors::AssetError::FungibleAssetAmountNotSufficient {
                minuend: MINT_AMOUNT,
                subtrahend: MINT_AMOUNT + 1,
            },
        ),
    )
    .await;
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn p2ide_transfer_consumed_by_target() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First Mint necessary token
    let note = mint_note(&mut client, from_account_id, faucet_account_id, NoteType::Private)
        .await
        .1;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    //Check that the note is not consumed by the target account
    assert!(matches!(
        client.get_input_note(note.id()).await.unwrap().unwrap().state(),
        InputNoteState::Committed { .. }
    ));

    consume_notes(&mut client, from_account_id, core::slice::from_ref(&note)).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    assert_account_has_single_asset(&client, from_account_id, faucet_account_id, MINT_AMOUNT).await;

    // Check that the note is consumed by the target account
    let input_note = client.get_input_note(note.id()).await.unwrap().unwrap();
    assert!(matches!(input_note.state(), InputNoteState::ConsumedAuthenticatedLocal { .. }));
    if let InputNoteState::ConsumedAuthenticatedLocal(ConsumedAuthenticatedLocalNoteState {
        submission_data,
        ..
    }) = input_note.state()
    {
        assert_eq!(submission_data.consumer_account, from_account_id);
    } else {
        panic!("Note should be consumed");
    }

    // Do a transfer from first account to second account with Recall. In this situation we'll do
    // the happy path where the `to_account_id` consumes the note
    let from_account_balance = client
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    let to_account_balance = client
        .account_reader(to_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    let current_block_num = client.get_sync_height().await.unwrap();
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    println!("Running P2IDE tx...");
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            )
            .with_reclaim_height(current_block_num + RECALL_HEIGHT_DELTA),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    Box::pin(client.submit_new_transaction(from_account_id, tx_request.clone()))
        .await
        .unwrap();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that note is committed for the second account to consume
    let notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(!notes.is_empty());

    // Make the `to_account_id` consume P2IDE note
    let note = tx_request.expected_output_own_notes().pop().unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone()])
        .unwrap();
    Box::pin(client.submit_new_transaction(to_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let from_status = client.account_reader(from_account_id).status().await.unwrap();
    // The seed should not be retrieved due to the account not being new
    assert!(!from_status.is_new() && from_status.seed().is_none());

    // Validate the transferred amounts
    let new_from_balance = client
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(
        new_from_balance,
        (from_account_balance - AssetAmount::new(TRANSFER_AMOUNT).unwrap()).unwrap()
    );

    let new_to_balance = client
        .account_reader(to_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(
        new_to_balance,
        (to_account_balance + AssetAmount::new(TRANSFER_AMOUNT).unwrap()).unwrap()
    );

    assert_note_cannot_be_consumed_twice(&mut client, to_account_id, note).await;
}

#[tokio::test]
async fn p2ide_transfer_consumed_by_sender() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First Mint necessary token
    mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Do a transfer from first account to second account with Recall. In this situation we'll do
    // the happy path where the `to_account_id` consumes the note
    let from_account_balance = client
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    let current_block_num = client.get_sync_height().await.unwrap();
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    println!("Running P2IDE tx...");
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            )
            .with_reclaim_height(current_block_num + RECALL_HEIGHT_DELTA),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();
    Box::pin(client.submit_new_transaction(from_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that note is committed
    println!("Fetching Committed Notes...");
    let notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(!notes.is_empty());

    // Check that it's still too early to consume
    println!("Consuming Note (too early)...");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![notes[0].clone().try_into().unwrap()])
        .unwrap();
    let transaction_execution_result =
        Box::pin(client.execute_transaction(from_account_id, tx_request)).await;
    assert!(transaction_execution_result.is_err_and(|err| {
        matches!(
            err,
            ClientError::TransactionExecutorError(
                TransactionExecutorError::TransactionProgramExecutionFailed(_)
            )
        )
    }));

    // Wait to consume with the sender account
    println!("Waiting for note to be consumable by sender");
    mock_rpc_api.advance_blocks(RECALL_HEIGHT_DELTA);
    client.sync_state().await.unwrap();

    // Consume the note with the sender account
    println!("Consuming Note...");
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![notes[0].clone().try_into().unwrap()])
        .unwrap();
    Box::pin(client.submit_new_transaction(from_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let from_status = client.account_reader(from_account_id).status().await.unwrap();
    // The seed should not be retrieved due to the account not being new
    assert!(!from_status.is_new() && from_status.seed().is_none());

    // Validate the sender hasn't lost funds
    let new_from_balance = client
        .account_reader(from_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(new_from_balance, from_account_balance);

    // Validate the target has no funds
    let to_balance = client
        .account_reader(to_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(to_balance, AssetAmount::ZERO);

    // Check that the target can't consume the note anymore
    assert_note_cannot_be_consumed_twice(
        &mut client,
        to_account_id,
        notes[0].clone().try_into().unwrap(),
    )
    .await;
}

#[tokio::test]
async fn p2ide_timelocked() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // First Mint necessary token
    mint_and_consume(&mut client, from_account_id, faucet_account_id, NoteType::Public).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let current_block_num = client.get_sync_height().await.unwrap();

    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            )
            .with_timelock_height(current_block_num + RECALL_HEIGHT_DELTA)
            .with_reclaim_height(current_block_num),
            NoteType::Public,
            client.rng(),
        )
        .unwrap();
    let note = tx_request.expected_output_own_notes().pop().unwrap();

    Box::pin(client.submit_new_transaction(from_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that it's still too early to consume by both accounts
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone()])
        .unwrap();
    let results = [
        Box::pin(client.execute_transaction(from_account_id, tx_request.clone())).await,
        Box::pin(client.execute_transaction(to_account_id, tx_request)).await,
    ];
    assert!(results.iter().all(|result| {
        result.as_ref().is_err_and(|err| {
            matches!(
                err,
                ClientError::TransactionExecutorError(
                    TransactionExecutorError::TransactionProgramExecutionFailed(_)
                )
            )
        })
    }));

    // Wait to consume with the target account
    mock_rpc_api.advance_blocks(RECALL_HEIGHT_DELTA);
    client.sync_state().await.unwrap();

    // Consume the note with the target account
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![note.clone()])
        .unwrap();
    Box::pin(client.submit_new_transaction(to_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let target_balance = client
        .account_reader(to_account_id)
        .get_balance(faucet_account_id)
        .await
        .unwrap();
    assert_eq!(target_balance, AssetAmount::new(TRANSFER_AMOUNT).unwrap());
}

#[tokio::test]
async fn get_consumable_notes() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;

    let (first_regular_account, second_regular_account, faucet_account_header) =
        setup_two_wallets_and_faucet(
            &mut client,
            AccountType::Private,
            &authenticator,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();

    let from_account_id = first_regular_account.id();
    let to_account_id = second_regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    //No consumable notes initially
    assert!(Box::pin(client.get_consumable_notes(None)).await.unwrap().is_empty());

    // First Mint necessary token
    let note = mint_note(&mut client, from_account_id, faucet_account_id, NoteType::Private)
        .await
        .1;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that note is consumable by the account that minted
    assert!(!Box::pin(client.get_consumable_notes(None)).await.unwrap().is_empty());
    assert!(
        !Box::pin(client.get_consumable_notes(Some(from_account_id)))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        Box::pin(client.get_consumable_notes(Some(to_account_id)))
            .await
            .unwrap()
            .is_empty()
    );

    consume_notes(&mut client, from_account_id, &[note]).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    //After consuming there are no more consumable notes
    assert!(Box::pin(client.get_consumable_notes(None)).await.unwrap().is_empty());

    // Do a transfer from first account to second account
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    println!("Running P2IDE tx...");
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                to_account_id,
            )
            .with_reclaim_height(100.into()),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    Box::pin(client.submit_new_transaction(from_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that note is consumable by both accounts
    let consumable_notes = Box::pin(client.get_consumable_notes(None)).await.unwrap();
    let relevant_accounts = &consumable_notes.first().unwrap().1;
    assert_eq!(relevant_accounts.len(), 2);
    assert!(
        !Box::pin(client.get_consumable_notes(Some(from_account_id)))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        !Box::pin(client.get_consumable_notes(Some(to_account_id)))
            .await
            .unwrap()
            .is_empty()
    );

    // Check that the note is only consumable after block 100 for the account that sent the
    // transaction
    let from_account_relevance = &relevant_accounts
        .iter()
        .find(|relevance| relevance.0 == from_account_id)
        .unwrap()
        .1;
    match from_account_relevance {
        NoteConsumptionStatus::ConsumableAfter(value) => {
            assert_eq!(value, &(100u32.into()));
        },
        _ => panic!("Unexpected NoteConsumptionStatus"),
    }

    // Check that the note is always consumable for the account that received the transaction
    let to_account_relevance = &relevant_accounts
        .iter()
        .find(|relevance| relevance.0 == to_account_id)
        .unwrap()
        .1;

    match to_account_relevance {
        NoteConsumptionStatus::Consumable
        | NoteConsumptionStatus::ConsumableAfter(..)
        | NoteConsumptionStatus::ConsumableWithAuthorization => {},
        _ => panic!("Unexpected NoteConsumptionStatus"),
    }
}

#[tokio::test]
async fn get_output_notes() {
    let (mut client, mock_rpc_api, authenticator) = Box::pin(create_test_client()).await;
    let _ = client.sync_state().await.unwrap();
    let (first_regular_account, faucet_account_header) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    let from_account_id = first_regular_account.id();
    let faucet_account_id = faucet_account_header.id();
    let random_account_id = AccountId::try_from(ACCOUNT_ID_REGULAR).unwrap();

    // No output notes initially
    assert!(client.get_output_notes(NoteFilter::All).await.unwrap().is_empty());

    // First Mint necessary token
    let note = mint_note(&mut client, from_account_id, faucet_account_id, NoteType::Private)
        .await
        .1;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that there was an output note but it wasn't consumed
    assert!(client.get_output_notes(NoteFilter::Consumed).await.unwrap().is_empty());
    assert!(!client.get_output_notes(NoteFilter::All).await.unwrap().is_empty());

    consume_notes(&mut client, from_account_id, &[note]).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // After consuming, the note is returned when using the [NoteFilter::Consumed] filter
    assert!(!client.get_input_notes(NoteFilter::Consumed).await.unwrap().is_empty());

    // Do a transfer from first account to second account
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    println!("Running P2ID tx...");
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(
                vec![Asset::Fungible(asset)],
                from_account_id,
                random_account_id,
            ),
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let output_note_id = tx_request.expected_output_own_notes().pop().unwrap().id();

    // Before executing, the output note is not found
    assert!(client.get_output_note(output_note_id).await.unwrap().is_none());

    Box::pin(client.submit_new_transaction(from_account_id, tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // After executing, the note is only found in output notes
    assert!(client.get_output_note(output_note_id).await.unwrap().is_some());
    assert!(client.get_input_note(output_note_id).await.unwrap().is_none());
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn account_rollback() {
    let (builder, mock_rpc_api, authenticator) = Box::pin(create_test_client_builder()).await;

    let mut client = builder.tx_discard_delta(Some(TX_DISCARD_DELTA)).build().await.unwrap();

    client.sync_state().await.unwrap();

    let (regular_account, faucet_account_header) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    let account_id = regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    // Mint a note
    let note = mint_note(&mut client, account_id, faucet_account_id, NoteType::Private).await.1;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    consume_notes(&mut client, account_id, &[note]).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Create a transaction but don't submit it to the node
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();

    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id, account_id),
            NoteType::Public,
            client.rng(),
        )
        .unwrap();

    // Execute the transaction but don't submit it to the node
    let transaction_result =
        Box::pin(client.execute_transaction(account_id, tx_request)).await.unwrap();
    let tx_id = transaction_result.id();

    // Store the account state before applying the transaction
    let account_commitment_before_tx =
        client.account_reader(account_id).commitment().await.unwrap();

    // Apply the transaction
    let submission_height = client.get_sync_height().await.unwrap();
    Box::pin(client.apply_transaction(&transaction_result, submission_height))
        .await
        .unwrap();

    // Check that the account state has changed after applying the transaction
    let account_commitment_after_tx = client.account_reader(account_id).commitment().await.unwrap();

    assert_ne!(
        account_commitment_before_tx, account_commitment_after_tx,
        "Account commitment should change after applying the transaction"
    );

    // Verify the transaction is in pending state
    let tx_record = client
        .get_transactions(TransactionFilter::All)
        .await
        .unwrap()
        .into_iter()
        .find(|tx| tx.id == tx_id)
        .unwrap();
    assert!(matches!(tx_record.status, TransactionStatus::Pending));

    // Sync the state, which should discard the old pending transaction
    mock_rpc_api.advance_blocks(TX_DISCARD_DELTA + 1);
    client.sync_state().await.unwrap();

    // Verify the transaction is now discarded
    let tx_record = client
        .get_transactions(TransactionFilter::All)
        .await
        .unwrap()
        .into_iter()
        .find(|tx| tx.id == tx_id)
        .unwrap();

    assert!(matches!(tx_record.status, TransactionStatus::Discarded(DiscardCause::Stale)));

    // Check that the account state has been rolled back after the transaction was discarded
    let account_commitment_after_sync =
        client.account_reader(account_id).commitment().await.unwrap();

    assert_ne!(
        account_commitment_after_sync, account_commitment_after_tx,
        "Account commitment should change after transaction was discarded"
    );
    assert_eq!(
        account_commitment_after_sync, account_commitment_before_tx,
        "Account commitment should be rolled back to the value before the transaction"
    );

    // Submit a new transaction after the rollback

    // Store the account state before applying the transaction
    let account_commitment_before_tx =
        client.account_reader(account_id).commitment().await.unwrap();

    // Apply a new transaction
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id, account_id),
            NoteType::Public,
            client.rng(),
        )
        .unwrap();
    let transaction_result =
        Box::pin(client.execute_transaction(account_id, tx_request)).await.unwrap();
    let tx_id = transaction_result.id();
    let submission_height = client.get_sync_height().await.unwrap();
    Box::pin(client.apply_transaction(&transaction_result, submission_height))
        .await
        .unwrap();

    // Check that the account state has changed after applying the transaction
    let account_commitment_after_tx = client.account_reader(account_id).commitment().await.unwrap();

    assert_ne!(
        account_commitment_after_tx, account_commitment_before_tx,
        "Account commitment should have changed after applying the new transaction"
    );

    // Submit the transaction
    let proven_transaction = client.prove_transaction(&transaction_result).await.unwrap();
    Box::pin(client.submit_proven_transaction(proven_transaction, &transaction_result))
        .await
        .unwrap();
    mock_rpc_api.prove_block();

    mock_rpc_api.advance_blocks(1);
    client.sync_state().await.unwrap();

    // Verify the transaction is now committed
    let tx_record = client
        .get_transactions(TransactionFilter::All)
        .await
        .unwrap()
        .into_iter()
        .find(|tx| tx.id == tx_id)
        .unwrap();

    assert!(matches!(tx_record.status, TransactionStatus::Committed { .. }));

    // Check that the account state has not been updated
    let account_commitment_after_sync =
        client.account_reader(account_id).commitment().await.unwrap();

    assert_ne!(
        account_commitment_after_sync, account_commitment_before_tx,
        "Account commitment should not have been rolled back after sync"
    );

    assert_eq!(
        account_commitment_after_sync, account_commitment_after_tx,
        "Account commitment should not have changed after sync"
    );
}

#[tokio::test]
async fn subsequent_discarded_transactions() {
    let (mut client, mock_rpc_api, keystore) = create_test_client().await;

    let (regular_account, faucet_account_header) =
        setup_wallet_and_faucet(&mut client, AccountType::Public, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    let account_id = regular_account.id();
    let faucet_account_id = faucet_account_header.id();

    let note = mint_note(&mut client, account_id, faucet_account_id, NoteType::Private).await.1;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    consume_notes(&mut client, account_id, &[note]).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Create a transaction that will expire in 2 blocks
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .expiration_delta(2)
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id, account_id),
            NoteType::Public,
            client.rng(),
        )
        .unwrap();

    // Execute the transaction but don't submit it to the node
    let transaction_result =
        Box::pin(client.execute_transaction(account_id, tx_request)).await.unwrap();
    let first_tx_id = transaction_result.id();

    let account_commitment_before_tx =
        client.account_reader(account_id).commitment().await.unwrap();

    let submission_height = client.get_sync_height().await.unwrap();
    Box::pin(client.apply_transaction(&transaction_result, submission_height))
        .await
        .unwrap();

    // Create a second transaction that will not expire
    let asset = FungibleAsset::new(faucet_account_id, TRANSFER_AMOUNT).unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .build_pay_to_id(
            PaymentNoteDescription::new(vec![Asset::Fungible(asset)], account_id, account_id),
            NoteType::Public,
            client.rng(),
        )
        .unwrap();

    // Execute the transaction but don't submit it to the node
    let transaction_result =
        Box::pin(client.execute_transaction(account_id, tx_request)).await.unwrap();
    let second_tx_id = transaction_result.id();
    let submission_height = client.get_sync_height().await.unwrap();
    Box::pin(client.apply_transaction(&transaction_result, submission_height))
        .await
        .unwrap();

    // Sync the state, which should discard the first transaction
    mock_rpc_api.advance_blocks(3);
    client.sync_state().await.unwrap();

    let account_commitment_after_sync =
        client.account_reader(account_id).commitment().await.unwrap();

    // Verify the first transaction is now discarded
    let first_tx_record = client
        .get_transactions(TransactionFilter::Ids(vec![first_tx_id]))
        .await
        .unwrap()
        .pop()
        .unwrap();

    assert!(matches!(
        first_tx_record.status,
        TransactionStatus::Discarded(DiscardCause::Expired)
    ));

    // Verify the second transaction is also discarded
    let second_tx_record = client
        .get_transactions(TransactionFilter::Ids(vec![second_tx_id]))
        .await
        .unwrap()
        .pop()
        .unwrap();

    println!("Second tx record: {:?}", second_tx_record.status);

    assert!(matches!(
        second_tx_record.status,
        TransactionStatus::Discarded(DiscardCause::DiscardedInitialState)
    ));

    // Check that the account state has been rolled back to the value before both transactions
    assert_eq!(account_commitment_after_sync, account_commitment_before_tx);
}

#[tokio::test]
async fn missing_recipient_digest() {
    let (mut client, _, keystore) = create_test_client().await;

    let faucet = insert_new_fungible_faucet(&mut client, AccountType::Private, &keystore)
        .await
        .unwrap();

    let dummy_recipient = NoteRecipient::new(
        Word::default(),
        StandardNote::SWAP.script(),
        NoteStorage::new(vec![]).unwrap(),
    );

    let dummy_recipient_digest = dummy_recipient.digest();

    let tx_request = TransactionRequestBuilder::new()
        .expected_output_recipients(vec![dummy_recipient])
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            AccountId::try_from(ACCOUNT_ID_PRIVATE_SENDER).unwrap(),
            NoteType::Public,
            client.rng(),
        )
        .unwrap();

    let error = Box::pin(client.submit_new_transaction(faucet.id(), tx_request))
        .await
        .unwrap_err();

    if let ClientError::MissingOutputRecipients(digests) = error {
        assert_eq!(digests, vec![dummy_recipient_digest]);
    }
}

#[tokio::test]
async fn input_note_checks() {
    let (mut client, mock_rpc_api, authenticator) = create_test_client().await;

    let (wallet, faucet) = setup_wallet_and_faucet(
        &mut client,
        AccountType::Private,
        &authenticator,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    let mut mint_notes = vec![];

    for _ in 0..5 {
        mint_notes.push(mint_note(&mut client, wallet.id(), faucet.id(), NoteType::Public).await.1);
        mock_rpc_api.prove_block();
        client.sync_state().await.unwrap();
    }

    let duplicate_note_tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![mint_notes[0].clone(), mint_notes[0].clone()]);

    assert!(matches!(
        duplicate_note_tx_request,
        Err(TransactionRequestError::DuplicateInputNote(note_id)) if note_id == mint_notes[0].id()
    ));

    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(mint_notes.clone())
        .unwrap();

    let transaction_result =
        Box::pin(client.execute_transaction(wallet.id(), tx_request)).await.unwrap();
    let transaction = transaction_result.executed_transaction().clone();

    let input_notes = transaction.input_notes().iter();

    // Check that the input notes have the same order as the original notes
    for (i, input_note) in input_notes.enumerate() {
        assert_eq!(input_note.id(), mint_notes[i].id());
    }

    let proven_transaction = client.prove_transaction(&transaction_result).await.unwrap();
    let submission_height = client
        .submit_proven_transaction(proven_transaction, &transaction_result)
        .await
        .unwrap();
    Box::pin(client.apply_transaction(&transaction_result, submission_height))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Check that using consumed notes will return an error
    let consumed_note_tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![mint_notes[0].clone()])
        .unwrap();
    let error = Box::pin(client.submit_new_transaction(wallet.id(), consumed_note_tx_request))
        .await
        .unwrap_err();

    assert!(matches!(
        error,
        ClientError::TransactionRequestError(TransactionRequestError::InputNoteAlreadyConsumed(_))
    ));
}

#[tokio::test]
async fn swap_chain_test() {
    // This test simulates a "swap chain" scenario with multiple wallets and fungible assets.
    // 1. It creates a number wallet/faucet pairs, each wallet holding an asset minted by its paired
    //    faucet.
    // 2. For each consecutive pair, it creates a swap transaction where wallet N offers its asset
    //    and requests the asset of wallet N+1.
    // 3. The last wallet, which didn't generate any swaps, holds the asset that the wallet N-1
    //    requested, which in turn was the asset requested by wallet N-2, and so on.
    // 4. The test then consumes all swap notes (in reverse order) in a single transaction against
    //    the last wallet.
    // 5. Although the last wallet doesn't contain any of the intermediate requested assets, it
    //    should be able to consume the swap notes because it will hold the requested asset for each
    //    step and gain the needed asset for the next. This can only happen if the notes are
    //    consumed in the specified order.
    // 6. Finally, it asserts that the last wallet now owns the asset originally held by the first
    //    wallet, verifying that the whole swap chain was successful.

    let (mut client, mock_rpc_api, keystore) = create_test_client().await;

    // Generate a few account pairs with a fungible asset that can be used for swaps.
    let mut account_pairs = vec![];
    for _ in 0..3 {
        let (wallet, faucet) = setup_wallet_and_faucet(
            &mut client,
            AccountType::Private,
            &keystore,
            RPO_FALCON_SCHEME_ID,
        )
        .await
        .unwrap();
        mint_and_consume(&mut client, wallet.id(), faucet.id(), NoteType::Private).await;
        mock_rpc_api.prove_block();
        client.sync_state().await.unwrap();

        account_pairs.push((wallet, faucet));
    }

    // Generate swap notes.
    // Except for the last, each wallet N will offer it's faucet N asset and request a faucet N+1
    // asset.
    let mut swap_notes = vec![];
    for pairs in account_pairs.windows(2) {
        let tx_request = TransactionRequestBuilder::new()
            .build_swap(
                &SwapTransactionData::new(
                    pairs[0].0.id(),
                    Asset::Fungible(FungibleAsset::new(pairs[0].1.id(), 1).unwrap()),
                    Asset::Fungible(FungibleAsset::new(pairs[1].1.id(), 1).unwrap()),
                ),
                NoteType::Private,
                NoteType::Private,
                client.rng(),
            )
            .unwrap();

        // The notes are inserted in reverse order because the first note to be consumed will be the
        // last one generated.
        swap_notes.insert(0, tx_request.expected_output_own_notes()[0].clone());
        Box::pin(client.submit_new_transaction(pairs[0].0.id(), tx_request))
            .await
            .unwrap();
        mock_rpc_api.prove_block();
        client.sync_state().await.unwrap();
    }

    // The last wallet didn't generate any swap notes and has the asset needed to start the swap
    // chain.
    let last_wallet = account_pairs.last().unwrap().0.id();

    // Trying to consume the notes in another order will fail.
    let tx_request = TransactionRequestBuilder::new()
        .build_consume_notes(swap_notes.iter().rev().cloned().collect())
        .unwrap();
    let error = Box::pin(client.submit_new_transaction(last_wallet, tx_request))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ClientError::TransactionExecutorError(
            TransactionExecutorError::TransactionProgramExecutionFailed(_)
        )
    ));

    let tx_request = TransactionRequestBuilder::new().build_consume_notes(swap_notes).unwrap();
    Box::pin(client.submit_new_transaction(last_wallet, tx_request)).await.unwrap();

    // At the end, the last wallet should have the asset of the first wallet.
    let last_wallet_balance = client
        .account_reader(last_wallet)
        .get_balance(account_pairs[0].1.id())
        .await
        .unwrap();
    assert_eq!(last_wallet_balance, AssetAmount::new(1).unwrap());
}

#[tokio::test]
async fn swap_public_payback_test() {
    let (mut client, mock_rpc_api, keystore) = create_test_client().await;

    let (wallet_a, faucet_a) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    mint_and_consume(&mut client, wallet_a.id(), faucet_a.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let (wallet_b, faucet_b) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();
    mint_and_consume(&mut client, wallet_b.id(), faucet_b.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // wallet_a offers asset_a and requests asset_b, with PUBLIC payback.
    let tx_request = TransactionRequestBuilder::new()
        .build_swap(
            &SwapTransactionData::new(
                wallet_a.id(),
                Asset::Fungible(FungibleAsset::new(faucet_a.id(), 1).unwrap()),
                Asset::Fungible(FungibleAsset::new(faucet_b.id(), 1).unwrap()),
            ),
            NoteType::Private,
            NoteType::Public,
            client.rng(),
        )
        .unwrap();

    let swap_note = tx_request.expected_output_own_notes()[0].clone();
    Box::pin(client.submit_new_transaction(wallet_a.id(), tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // wallet_b consumes the swap, producing a public P2ID payback to wallet_a with no off-band
    // data.
    let tx_request = TransactionRequestBuilder::new().build_consume_notes(vec![swap_note]).unwrap();
    Box::pin(client.submit_new_transaction(wallet_b.id(), tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // wallet_b ended up with asset_a, wallet_a will receive asset_b via the public payback note.
    let wallet_b_balance =
        client.account_reader(wallet_b.id()).get_balance(faucet_a.id()).await.unwrap();
    assert_eq!(wallet_b_balance, AssetAmount::new(1).unwrap());
}

/// Tests that partial output notes (created when a SWAP note is consumed) are correctly included in
/// `NoteFilter::Unspent` and receive inclusion proofs during sync, transitioning from
/// `ExpectedPartial` to `CommittedPartial` state.
///
/// This is a regression test for a bug where `NoteFilter::Unspent` for output notes did not
/// include `ExpectedPartial` and `CommittedPartial` states, causing partial output notes to be
/// excluded from sync operations and never receiving their inclusion proofs.
#[tokio::test]
async fn partial_output_note_receives_inclusion_proof_after_sync() {
    let (mut client, mock_rpc_api, keystore) = Box::pin(create_test_client()).await;
    client.sync_state().await.unwrap();

    // Set up two wallet-faucet pairs for the swap scenario.
    let (wallet_a, faucet_a) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    let (wallet_b, faucet_b) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    // Mint and consume tokens so each wallet holds assets for the swap.
    mint_and_consume(&mut client, wallet_a.id(), faucet_a.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    mint_and_consume(&mut client, wallet_b.id(), faucet_b.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Wallet A creates a SWAP note: offers 1 unit of faucet_a, requests 1 unit of faucet_b.
    let offered_asset = Asset::Fungible(FungibleAsset::new(faucet_a.id(), 1).unwrap());
    let requested_asset = Asset::Fungible(FungibleAsset::new(faucet_b.id(), 1).unwrap());

    let swap_tx_request = TransactionRequestBuilder::new()
        .build_swap(
            &SwapTransactionData::new(wallet_a.id(), offered_asset, requested_asset),
            NoteType::Private,
            NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let swap_note = swap_tx_request.expected_output_own_notes()[0].clone();

    Box::pin(client.submit_new_transaction(wallet_a.id(), swap_tx_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Wallet B consumes the SWAP note. The SWAP script derives the payback recipient at consume
    // time (P2ID to the creator with serial = SWAP_serial with element 0 + 1) and emits the
    // payback note. From the VM's perspective this payback note is an OutputNote::Partial, which
    // gets stored as ExpectedPartial.
    let consume_tx_request =
        TransactionRequestBuilder::new().build_consume_notes(vec![swap_note]).unwrap();

    Box::pin(client.submit_new_transaction(wallet_b.id(), consume_tx_request))
        .await
        .unwrap();

    // Before the block is proven and synced, the payback note should be tracked as ExpectedPartial.
    // The fix in filters.rs ensures NoteFilter::Unspent includes ExpectedPartial notes; without it
    // this query would return an empty list for partial notes.
    let unspent_before_sync = client.get_output_notes(NoteFilter::Unspent).await.unwrap();
    let expected_partial_count = unspent_before_sync
        .iter()
        .filter(|n| matches!(n.state(), OutputNoteState::ExpectedPartial))
        .count();
    assert!(
        expected_partial_count > 0,
        "Expected at least one output note in ExpectedPartial state before sync, found 0"
    );

    // Prove the block (commits wallet B's transaction with the partial payback note).
    mock_rpc_api.prove_block();

    // Sync to receive inclusion proofs. With the fix, the ExpectedPartial output note is included
    // in the NoteFilter::Unspent query used by sync, so it receives an inclusion proof and
    // transitions to CommittedPartial.
    client.sync_state().await.unwrap();

    // After sync, the partial note should have an inclusion proof (CommittedPartial) and still
    // appear under NoteFilter::Unspent.
    let unspent_after_sync = client.get_output_notes(NoteFilter::Unspent).await.unwrap();
    let committed_partial_count = unspent_after_sync
        .iter()
        .filter(|n| matches!(n.state(), OutputNoteState::CommittedPartial { .. }))
        .count();
    assert!(
        committed_partial_count > 0,
        "Expected at least one output note in CommittedPartial state after sync, found 0"
    );

    // The note must no longer be stuck in ExpectedPartial — it received its inclusion proof.
    let remaining_expected_partial = unspent_after_sync
        .iter()
        .filter(|n| matches!(n.state(), OutputNoteState::ExpectedPartial))
        .count();
    assert_eq!(
        remaining_expected_partial, 0,
        "All ExpectedPartial notes should have transitioned to CommittedPartial after sync"
    );
}

// Verifies that Alice can create a PSWAP note offering ETH for USD, and Bob can fill it. With
// a full fill (`account_fill_amount == requested_amount`) no remainder is produced; with a
// partial fill, Bob receives a proportional payout and a remainder PSWAP note is produced
// carrying the unfilled amounts.
#[rstest]
#[case::full_fill(100, 50, 50, 100, None)]
#[case::partial_fill(100, 50, 25, 50, Some((50, 25)))]
#[tokio::test]
async fn pswap_fill_test(
    #[case] offered_amount: u64,
    #[case] requested_amount: u64,
    #[case] account_fill_amount: u64,
    #[case] expected_payout: u64,
    #[case] expected_remainder: Option<(u64, u64)>,
) {
    let (mut client, mock_rpc_api, keystore) = Box::pin(create_test_client()).await;

    // Setup Alice's wallet and the ETH faucet (offered asset).
    let (alice_wallet, eth_faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    // Setup Bob's wallet and the USD faucet (requested asset).
    let (bob_wallet, usd_faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    mint_and_consume(&mut client, alice_wallet.id(), eth_faucet.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    mint_and_consume(&mut client, bob_wallet.id(), usd_faucet.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Step 1: Alice creates a PSWAP note offering ETH for USD.
    let pswap_data = PswapTransactionData::new(
        alice_wallet.id(),
        FungibleAsset::new(eth_faucet.id(), offered_amount).unwrap(),
        FungibleAsset::new(usd_faucet.id(), requested_amount).unwrap(),
    );

    let create_request = TransactionRequestBuilder::new()
        .build_pswap_create(&pswap_data, NoteType::Private, NoteType::Private, None, client.rng())
        .unwrap();

    let pswap_note = create_request.expected_output_own_notes()[0].clone();
    Box::pin(client.submit_new_transaction(alice_wallet.id(), create_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Alice's ETH balance should decrease by the full offered amount regardless of fill.
    let alice_account = client.get_account(alice_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(eth_faucet.id()))
            .unwrap(),
        AssetAmount::new(MINT_AMOUNT - offered_amount).unwrap(),
        "Alice's ETH balance should decrease by the offered amount"
    );

    // Step 2: Bob fills the PSWAP note.
    let consume_request = TransactionRequestBuilder::new()
        .build_pswap_consume(
            &pswap_note,
            bob_wallet.id(),
            AssetAmount::new(account_fill_amount).unwrap(),
            AssetAmount::ZERO,
        )
        .unwrap();

    Box::pin(client.submit_new_transaction(bob_wallet.id(), consume_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let bob_account = client.get_account(bob_wallet.id()).await.unwrap().unwrap();

    // Bob spent exactly the fill amount — proves NOTE_ARGS were honored (a wrong layout would
    // fall back to the script's full-fill default path).
    assert_eq!(
        bob_account.vault().get_balance(AssetId::new_fungible(usd_faucet.id())).unwrap(),
        AssetAmount::new(MINT_AMOUNT - account_fill_amount).unwrap(),
        "Bob's USD balance should decrease by exactly the fill amount"
    );

    // Bob received the proportional payout.
    assert_eq!(
        bob_account.vault().get_balance(AssetId::new_fungible(eth_faucet.id())).unwrap(),
        AssetAmount::new(expected_payout).unwrap(),
        "Bob should have received the expected ETH payout"
    );

    // The remainder note is produced only on partial fills.
    let all_notes = client.get_input_notes(NoteFilter::All).await.unwrap();
    let remainder = all_notes.iter().find_map(|record| {
        let note: Note = record.try_into().ok()?;
        if note.id() == pswap_note.id() {
            return None;
        }
        PswapNote::try_from(&note).ok()
    });

    match expected_remainder {
        Some((rem_offered, rem_requested)) => {
            let remainder =
                remainder.expect("remainder PSWAP note should exist after partial fill");
            assert_eq!(
                remainder.offered_asset().amount().as_u64(),
                rem_offered,
                "remainder offered amount should reflect the unfilled portion"
            );
            assert_eq!(
                remainder.storage().min_requested_amount(),
                rem_requested,
                "remainder requested amount should reflect the unfilled portion"
            );
        },
        None => {
            assert!(remainder.is_none(), "no remainder PSWAP note should exist after a full fill");
        },
    }
}

#[tokio::test]
async fn pswap_cancel_test() {
    // This test verifies that:
    // 1. Alice creates a PSWAP note (balance decreases).
    // 2. Alice cancels the PSWAP note (balance restored).

    let (mut client, mock_rpc_api, keystore) = Box::pin(create_test_client()).await;

    let (alice_wallet, eth_faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    let (_bob_wallet, usd_faucet) =
        setup_wallet_and_faucet(&mut client, AccountType::Private, &keystore, RPO_FALCON_SCHEME_ID)
            .await
            .unwrap();

    mint_and_consume(&mut client, alice_wallet.id(), eth_faucet.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Step 1: Alice creates a PSWAP note offering ETH for USD.
    let offered_amount = 100u64;
    let requested_amount = 50u64;
    let pswap_data = PswapTransactionData::new(
        alice_wallet.id(),
        FungibleAsset::new(eth_faucet.id(), offered_amount).unwrap(),
        FungibleAsset::new(usd_faucet.id(), requested_amount).unwrap(),
    );

    let create_request = TransactionRequestBuilder::new()
        .build_pswap_create(&pswap_data, NoteType::Private, NoteType::Private, None, client.rng())
        .unwrap();

    let pswap_note = create_request.expected_output_own_notes()[0].clone();
    Box::pin(client.submit_new_transaction(alice_wallet.id(), create_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Verify Alice's balance decreased after creating the PSWAP note.
    let alice_account = client.get_account(alice_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(eth_faucet.id()))
            .unwrap(),
        AssetAmount::new(MINT_AMOUNT - offered_amount).unwrap(),
        "Alice's ETH balance should decrease by the offered amount"
    );

    // Step 2: Alice cancels the PSWAP note.
    let cancel_request = TransactionRequestBuilder::new()
        .build_pswap_cancel(pswap_note, alice_wallet.id())
        .unwrap();

    Box::pin(client.submit_new_transaction(alice_wallet.id(), cancel_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // Verify Alice's balance is restored after canceling the PSWAP note.
    let alice_account = client.get_account(alice_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(eth_faucet.id()))
            .unwrap(),
        AssetAmount::new(MINT_AMOUNT).unwrap(),
        "Alice's ETH balance should be fully restored after canceling the PSWAP note"
    );
}

// Builds a client backed by the given shared mock chain. Cloning a `MockRpcApi`
// shares its `Arc<RwLock<MockChain>>`, so every client built this way transacts
// against — and syncs from — the same chain while keeping its own store and
// keystore. This is what lets the PSWAP lineage test model Alice and Bob as two
// genuinely separate clients (as they are in production), rather than two
// accounts colocated on one store.
async fn create_pswap_test_client(
    mock_rpc_api: &MockRpcApi,
) -> (MockClient<FilesystemKeyStore>, FilesystemKeyStore) {
    let mut seed_rng = rand::rng();
    let coin_seed: [u64; 4] = seed_rng.random();
    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());

    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();

    let mut client = ClientBuilder::new()
        .rpc(Arc::new(mock_rpc_api.clone()))
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore.clone()))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    (client, keystore)
}

/// Two-client mock-chain test: Alice creates a PSWAP, Bob partial-fills, Alice reclaims the
/// remainder and consumes the payback. Runs as `#[rstest]` cases for `NoteType::Public` and
/// `NoteType::Private`; for the private case the test pre-registers Bob's payback + remainder
/// attachments on the mock RPC (a real node returns them automatically).
#[rstest]
#[case::public_pswap(NoteType::Public)]
#[case::private_pswap(NoteType::Private)]
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn pswap_chain_tracking_test(#[case] note_type: NoteType) {
    // One shared chain, two independent clients.
    let mock_rpc_api = MockRpcApi::new(Box::pin(create_prebuilt_mock_chain()).await);
    let (mut alice_client, alice_keystore) = create_pswap_test_client(&mock_rpc_api).await;
    let (mut bob_client, bob_keystore) = create_pswap_test_client(&mock_rpc_api).await;

    let (alice_wallet, btc_faucet) = setup_wallet_and_faucet(
        &mut alice_client,
        AccountType::Private,
        &alice_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    let (bob_wallet, eth_faucet) = setup_wallet_and_faucet(
        &mut bob_client,
        AccountType::Private,
        &bob_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    mint_and_consume(&mut alice_client, alice_wallet.id(), btc_faucet.id(), NoteType::Private)
        .await;
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    mint_and_consume(&mut bob_client, bob_wallet.id(), eth_faucet.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    bob_client.sync_state().await.unwrap();

    // ── Alice creates the PSWAP offering 100 BTC for 50 ETH (2:1 rate). ──
    let offered_amount = 100u64;
    let requested_amount = 50u64;
    let offered_asset = FungibleAsset::new(btc_faucet.id(), offered_amount).unwrap();
    let requested_asset = FungibleAsset::new(eth_faucet.id(), requested_amount).unwrap();
    let pswap_data = PswapTransactionData::new(alice_wallet.id(), offered_asset, requested_asset);

    let create_request = TransactionRequestBuilder::new()
        .build_pswap_create(&pswap_data, note_type, note_type, None, alice_client.rng())
        .unwrap();

    let pswap_note = create_request.expected_output_own_notes()[0].clone();
    let pswap_typed = PswapNote::try_from(&pswap_note).unwrap();
    let order_id = pswap_typed.order_id();

    Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), create_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    // Public-only: Bob discovers the order via the asset-pair tag. Private orders are
    // exchanged off-chain (the test hands `pswap_note` to Bob directly).
    if note_type == NoteType::Public {
        let pswap_tag = PswapNote::create_tag(note_type, &offered_asset, &requested_asset);
        bob_client.add_note_tag(pswap_tag).await.unwrap();
        bob_client.sync_state().await.unwrap();
    }

    // The creation hook installs a depth-0 Active lineage with the full amounts.
    let lineage = alice_client
        .pswap_lineage(order_id)
        .await
        .unwrap()
        .expect("creating a PSWAP should install a lineage row");
    assert_eq!(lineage.current_depth, 0);
    assert_eq!(lineage.state, PswapLineageState::Active);
    assert_eq!(lineage.remaining_offered.as_u64(), offered_amount);
    assert_eq!(lineage.remaining_requested.as_u64(), requested_amount);

    // Private-only: pre-register payback + remainder attachments on the mock (a real node
    // returns them via RPC).
    if note_type == NoteType::Private {
        let fill_amount = AssetAmount::new(25).unwrap();
        let payout_amount = AssetAmount::new(50).unwrap();
        let new_offered = AssetAmount::new(50).unwrap();
        let new_requested = AssetAmount::new(25).unwrap();
        let payback_attachment = PswapNoteAttachment::new(fill_amount, order_id, 1);
        let remainder_attachment = PswapNoteAttachment::new(payout_amount, order_id, 1);
        let expected_payback =
            pswap_typed.payback_note(bob_wallet.id(), &payback_attachment).unwrap();
        let expected_remainder = pswap_typed
            .remainder_note(bob_wallet.id(), &remainder_attachment, new_offered, new_requested)
            .unwrap();
        mock_rpc_api.register_private_note_attachments(
            expected_payback.id(),
            expected_payback.attachments().clone(),
        );
        mock_rpc_api.register_private_note_attachments(
            expected_remainder.id(),
            expected_remainder.attachments().clone(),
        );
    }

    // ── Bob partial-fills: 25 ETH → 50 BTC payout, leaving 50 BTC / 25 ETH. ──
    let consume_request = TransactionRequestBuilder::new()
        .build_pswap_consume(
            &pswap_note,
            bob_wallet.id(),
            AssetAmount::new(25).unwrap(),
            AssetAmount::ZERO,
        )
        .unwrap();
    Box::pin(bob_client.submit_new_transaction(bob_wallet.id(), consume_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    bob_client.sync_state().await.unwrap();
    alice_client.sync_state().await.unwrap();

    let lineage = alice_client.pswap_lineage(order_id).await.unwrap().unwrap();
    assert_eq!(lineage.current_depth, 1, "fill should advance the tip to depth 1");
    assert_eq!(lineage.state, PswapLineageState::Active);
    assert_eq!(lineage.remaining_offered.as_u64(), 50);
    assert_eq!(lineage.remaining_requested.as_u64(), 25);

    // Payback must land `Committed` (not `Unverified`) on Alice's side. For the public case
    // the standard screener path inserts it; for the private case the at_block_header path in
    // apply_pswap_round does. Either way, the row should be immediately consumable.
    let payback_attachment = PswapNoteAttachment::new(AssetAmount::new(25).unwrap(), order_id, 1);
    let payback_id = pswap_typed.payback_note(bob_wallet.id(), &payback_attachment).unwrap().id();
    let payback_record = alice_client
        .get_input_notes(NoteFilter::Unique(payback_id))
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("payback must be tracked in alice's input_notes");
    assert!(payback_record.is_committed(), "payback must land Committed");

    // ── Reclaim: Alice cancels the depth-1 tip via the order id. ──
    let cancel_request = alice_client.build_pswap_cancel_by_order(order_id).await.unwrap();
    Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), cancel_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    let lineage = alice_client.pswap_lineage(order_id).await.unwrap().unwrap();
    assert_eq!(
        lineage.state,
        PswapLineageState::Reclaimed,
        "reclaiming the depth-1 tip should terminate the lineage"
    );

    // Terminal state must drop the asset-pair subscription.
    let asset_pair_tag = PswapNote::create_tag(note_type, &offered_asset, &requested_asset);
    assert!(
        !alice_client
            .get_note_tags()
            .await
            .unwrap()
            .iter()
            .any(|r| r.tag == asset_pair_tag),
        "asset-pair tag should be unsubscribed after Reclaimed"
    );

    // Alice consumes the ETH payback Bob's fill produced.
    let consumable = alice_client.get_consumable_notes(Some(alice_wallet.id())).await.unwrap();
    let payback_notes: Vec<Note> =
        consumable.iter().map(|(record, _)| record.try_into().unwrap()).collect();
    assert_eq!(payback_notes.len(), 1, "Alice should hold one ETH payback note");
    consume_notes(&mut alice_client, alice_wallet.id(), &payback_notes).await;
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    let alice_account = alice_client.get_account(alice_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(eth_faucet.id()))
            .unwrap(),
        AssetAmount::new(25).unwrap(),
        "Alice should have received 25 ETH from the fill"
    );

    // BTC: 100 locked − 50 to Bob + 50 reclaimed = MINT − 50.
    let alice_account = alice_client.get_account(alice_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(btc_faucet.id()))
            .unwrap(),
        AssetAmount::new(MINT_AMOUNT - 50).unwrap(),
        "Alice's BTC should reflect 50 paid out and 50 reclaimed"
    );
}

/// Full-fill counterpart to [`pswap_chain_tracking_test`]. Bob consumes the entire requested
/// side in one transaction, so the script emits ONLY a payback (no remainder). Lineage moves
/// `Active → FullyFilled`, the asset-pair tag drops, and Alice consumes the full payback.
#[rstest]
#[case::public_pswap(NoteType::Public)]
#[case::private_pswap(NoteType::Private)]
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn pswap_full_fill_chain_tracking_test(#[case] note_type: NoteType) {
    let mock_rpc_api = MockRpcApi::new(Box::pin(create_prebuilt_mock_chain()).await);
    let (mut alice_client, alice_keystore) = create_pswap_test_client(&mock_rpc_api).await;
    let (mut bob_client, bob_keystore) = create_pswap_test_client(&mock_rpc_api).await;

    let (alice_wallet, btc_faucet) = setup_wallet_and_faucet(
        &mut alice_client,
        AccountType::Private,
        &alice_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();
    let (bob_wallet, eth_faucet) = setup_wallet_and_faucet(
        &mut bob_client,
        AccountType::Private,
        &bob_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    mint_and_consume(&mut alice_client, alice_wallet.id(), btc_faucet.id(), NoteType::Private)
        .await;
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();
    mint_and_consume(&mut bob_client, bob_wallet.id(), eth_faucet.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    bob_client.sync_state().await.unwrap();

    let offered_amount = 100u64;
    let requested_amount = 50u64;
    let offered_asset = FungibleAsset::new(btc_faucet.id(), offered_amount).unwrap();
    let requested_asset = FungibleAsset::new(eth_faucet.id(), requested_amount).unwrap();
    let pswap_data = PswapTransactionData::new(alice_wallet.id(), offered_asset, requested_asset);

    let create_request = TransactionRequestBuilder::new()
        .build_pswap_create(&pswap_data, note_type, note_type, None, alice_client.rng())
        .unwrap();
    let pswap_note = create_request.expected_output_own_notes()[0].clone();
    let pswap_typed = PswapNote::try_from(&pswap_note).unwrap();
    let order_id = pswap_typed.order_id();

    Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), create_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    if note_type == NoteType::Public {
        let pswap_tag = PswapNote::create_tag(note_type, &offered_asset, &requested_asset);
        bob_client.add_note_tag(pswap_tag).await.unwrap();
        bob_client.sync_state().await.unwrap();
    }

    // Private-only: pre-register payback attachment (full fill emits no remainder, so only
    // one attachment to register).
    if note_type == NoteType::Private {
        let fill_amount = AssetAmount::new(requested_amount).unwrap();
        let payback_attachment = PswapNoteAttachment::new(fill_amount, order_id, 1);
        let expected_payback =
            pswap_typed.payback_note(bob_wallet.id(), &payback_attachment).unwrap();
        mock_rpc_api.register_private_note_attachments(
            expected_payback.id(),
            expected_payback.attachments().clone(),
        );
    }

    // Bob full-fills: consumes the entire 50 ETH side → only a payback note is emitted.
    let consume_request = TransactionRequestBuilder::new()
        .build_pswap_consume(
            &pswap_note,
            bob_wallet.id(),
            AssetAmount::new(requested_amount).unwrap(),
            AssetAmount::ZERO,
        )
        .unwrap();
    Box::pin(bob_client.submit_new_transaction(bob_wallet.id(), consume_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    bob_client.sync_state().await.unwrap();
    alice_client.sync_state().await.unwrap();

    let lineage = alice_client.pswap_lineage(order_id).await.unwrap().unwrap();
    assert_eq!(lineage.current_depth, 1, "full fill is a single round");
    assert_eq!(lineage.state, PswapLineageState::FullyFilled);
    assert_eq!(lineage.remaining_offered.as_u64(), 0);
    assert_eq!(lineage.remaining_requested.as_u64(), 0);

    // FullyFilled must drop the asset-pair subscription (same code path as Reclaimed).
    let asset_pair_tag = PswapNote::create_tag(note_type, &offered_asset, &requested_asset);
    assert!(
        !alice_client
            .get_note_tags()
            .await
            .unwrap()
            .iter()
            .any(|r| r.tag == asset_pair_tag),
        "asset-pair tag should be unsubscribed after FullyFilled"
    );

    // Alice consumes the full ETH payback (50 ETH — the full requested side).
    let consumable = alice_client.get_consumable_notes(Some(alice_wallet.id())).await.unwrap();
    let payback_notes: Vec<Note> =
        consumable.iter().map(|(record, _)| record.try_into().unwrap()).collect();
    assert_eq!(payback_notes.len(), 1, "Alice should hold one ETH payback note");
    consume_notes(&mut alice_client, alice_wallet.id(), &payback_notes).await;
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    let alice_account = alice_client.get_account(alice_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(eth_faucet.id()))
            .unwrap(),
        AssetAmount::new(requested_amount).unwrap(),
        "Alice should have received the full 50 ETH"
    );
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(btc_faucet.id()))
            .unwrap(),
        AssetAmount::new(MINT_AMOUNT - offered_amount).unwrap(),
        "Alice's BTC should reflect the full 100 paid out"
    );
}

/// Cross-sync multi-round lineage tracking. Bob fills the same order twice in *separate* blocks,
/// consuming his own round-1 remainder as the round-2 input. This is the scenario the
/// `build_pswap_consume` fix unblocks: a consumer's fill registers no expected future notes, so the
/// remainder is no longer left in Bob's store as a proofless, un-consumable duplicate. After round
/// 1 it instead lands in Bob's store as a Committed note via the output-note screening path, which
/// makes it cleanly consumable in round 2. Alice's lineage must walk depth 0 → 1 → 2 and then
/// Reclaim the final tip.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn pswap_multi_round_chain_tracking_test() {
    let note_type = NoteType::Public;
    let mock_rpc_api = MockRpcApi::new(Box::pin(create_prebuilt_mock_chain()).await);
    let (mut alice_client, alice_keystore) = create_pswap_test_client(&mock_rpc_api).await;
    let (mut bob_client, bob_keystore) = create_pswap_test_client(&mock_rpc_api).await;

    let (alice_wallet, btc_faucet) = setup_wallet_and_faucet(
        &mut alice_client,
        AccountType::Private,
        &alice_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();
    let (bob_wallet, eth_faucet) = setup_wallet_and_faucet(
        &mut bob_client,
        AccountType::Private,
        &bob_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    mint_and_consume(&mut alice_client, alice_wallet.id(), btc_faucet.id(), NoteType::Private)
        .await;
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();
    mint_and_consume(&mut bob_client, bob_wallet.id(), eth_faucet.id(), NoteType::Private).await;
    mock_rpc_api.prove_block();
    bob_client.sync_state().await.unwrap();

    // ── Alice creates the PSWAP offering 100 BTC for 50 ETH (2:1 rate). ──
    let offered_amount = 100u64;
    let requested_amount = 50u64;
    let offered_asset = FungibleAsset::new(btc_faucet.id(), offered_amount).unwrap();
    let requested_asset = FungibleAsset::new(eth_faucet.id(), requested_amount).unwrap();
    let pswap_data = PswapTransactionData::new(alice_wallet.id(), offered_asset, requested_asset);

    let create_request = TransactionRequestBuilder::new()
        .build_pswap_create(&pswap_data, note_type, note_type, None, alice_client.rng())
        .unwrap();
    let pswap_note = create_request.expected_output_own_notes()[0].clone();
    let pswap_typed = PswapNote::try_from(&pswap_note).unwrap();
    let order_id = pswap_typed.order_id();

    Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), create_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    // Bob discovers the public order via the asset-pair tag.
    let pswap_tag = PswapNote::create_tag(note_type, &offered_asset, &requested_asset);
    bob_client.add_note_tag(pswap_tag).await.unwrap();
    bob_client.sync_state().await.unwrap();

    // ── Round 1: Bob fills 25 ETH → 50 BTC payout, leaving 50 BTC / 25 ETH. ──
    let consume_request = TransactionRequestBuilder::new()
        .build_pswap_consume(
            &pswap_note,
            bob_wallet.id(),
            AssetAmount::new(25).unwrap(),
            AssetAmount::ZERO,
        )
        .unwrap();
    Box::pin(bob_client.submit_new_transaction(bob_wallet.id(), consume_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    bob_client.sync_state().await.unwrap();
    alice_client.sync_state().await.unwrap();

    let lineage = alice_client.pswap_lineage(order_id).await.unwrap().unwrap();
    assert_eq!(lineage.current_depth, 1, "round 1 advances the tip to depth 1");
    assert_eq!(lineage.state, PswapLineageState::Active);
    assert_eq!(lineage.remaining_offered.as_u64(), 50);
    assert_eq!(lineage.remaining_requested.as_u64(), 25);

    // Bob's round-1 remainder must be tracked as a consumable note in his own store — this is
    // exactly what the fix enables. Previously it was a proofless `expected_future_notes`
    // duplicate that could never be consumed, blocking round 2.
    let bob_consumable = bob_client.get_consumable_notes(Some(bob_wallet.id())).await.unwrap();
    let remainder_r1 = bob_consumable
        .iter()
        .find_map(|(record, _)| {
            let note: Note = record.try_into().ok()?;
            PswapNote::try_from(&note).ok()?;
            Some(note)
        })
        .expect("Bob should hold his round-1 remainder as a consumable PSWAP note");

    // ── Round 2 (separate block): Bob consumes his own remainder, filling 10 ETH → 20 BTC,
    //    leaving 30 BTC / 15 ETH. ──
    let consume_request = TransactionRequestBuilder::new()
        .build_pswap_consume(
            &remainder_r1,
            bob_wallet.id(),
            AssetAmount::new(10).unwrap(),
            AssetAmount::ZERO,
        )
        .unwrap();
    Box::pin(bob_client.submit_new_transaction(bob_wallet.id(), consume_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    bob_client.sync_state().await.unwrap();
    alice_client.sync_state().await.unwrap();

    let lineage = alice_client.pswap_lineage(order_id).await.unwrap().unwrap();
    assert_eq!(lineage.current_depth, 2, "round 2 advances the tip to depth 2");
    assert_eq!(lineage.state, PswapLineageState::Active);
    assert_eq!(lineage.remaining_offered.as_u64(), 30);
    assert_eq!(lineage.remaining_requested.as_u64(), 15);

    // ── Reclaim: Alice cancels the depth-2 tip via the order id. ──
    let cancel_request = alice_client.build_pswap_cancel_by_order(order_id).await.unwrap();
    Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), cancel_request))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    let lineage = alice_client.pswap_lineage(order_id).await.unwrap().unwrap();
    assert_eq!(
        lineage.state,
        PswapLineageState::Reclaimed,
        "reclaiming the depth-2 tip should terminate the lineage"
    );

    // Alice consumes both ETH paybacks (25 + 10 = 35 ETH).
    let consumable = alice_client.get_consumable_notes(Some(alice_wallet.id())).await.unwrap();
    let payback_notes: Vec<Note> =
        consumable.iter().map(|(record, _)| record.try_into().unwrap()).collect();
    assert_eq!(payback_notes.len(), 2, "Alice should hold two ETH paybacks (one per round)");
    consume_notes(&mut alice_client, alice_wallet.id(), &payback_notes).await;
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    let alice_account = alice_client.get_account(alice_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(eth_faucet.id()))
            .unwrap(),
        AssetAmount::new(35).unwrap(),
        "Alice should have received 25 + 10 = 35 ETH across both rounds"
    );
    // BTC: 100 locked − 70 paid out (50 + 20) + 30 reclaimed = MINT − 70.
    assert_eq!(
        alice_account
            .vault()
            .get_balance(AssetId::new_fungible(btc_faucet.id()))
            .unwrap(),
        AssetAmount::new(MINT_AMOUNT - 70).unwrap(),
        "Alice's BTC should reflect 70 paid out and 30 reclaimed"
    );

    // Bob received 50 + 20 = 70 BTC and paid 25 + 10 = 35 ETH across both fills.
    let bob_account = bob_client.get_account(bob_wallet.id()).await.unwrap().unwrap();
    assert_eq!(
        bob_account.vault().get_balance(AssetId::new_fungible(btc_faucet.id())).unwrap(),
        AssetAmount::new(70).unwrap(),
        "Bob should have received 70 BTC across both fills"
    );
    assert_eq!(
        bob_account.vault().get_balance(AssetId::new_fungible(eth_faucet.id())).unwrap(),
        AssetAmount::new(MINT_AMOUNT - 35).unwrap(),
        "Bob should have paid 35 ETH across both fills"
    );
}

/// Two PSWAP orders for the same asset pair share one asset-pair tag (one `Subscription` row
/// per order). Terminating one must not cancel the other's subscription; only when the LAST
/// order on the pair terminates does the tag drop out of the client's tracked-tag set.
#[tokio::test]
async fn pswap_asset_pair_tag_isolated_per_order() {
    let mock_rpc_api = MockRpcApi::new(Box::pin(create_prebuilt_mock_chain()).await);
    let (mut alice_client, alice_keystore) = create_pswap_test_client(&mock_rpc_api).await;

    let (alice_wallet, btc_faucet) = setup_wallet_and_faucet(
        &mut alice_client,
        AccountType::Private,
        &alice_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();
    // Second faucet only — its `_throwaway_wallet` is unused; we just need the ETH faucet id.
    let (_throwaway_wallet, eth_faucet) = setup_wallet_and_faucet(
        &mut alice_client,
        AccountType::Private,
        &alice_keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    mint_and_consume(&mut alice_client, alice_wallet.id(), btc_faucet.id(), NoteType::Public).await;
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();

    // Helper: build, submit, and sync one PSWAP. Returns the order id.
    let mut create_order = async |offered: u64, requested: u64| -> Felt {
        let data = PswapTransactionData::new(
            alice_wallet.id(),
            FungibleAsset::new(btc_faucet.id(), offered).unwrap(),
            FungibleAsset::new(eth_faucet.id(), requested).unwrap(),
        );
        let request = TransactionRequestBuilder::new()
            .build_pswap_create(&data, NoteType::Public, NoteType::Public, None, alice_client.rng())
            .unwrap();
        let note = request.expected_output_own_notes()[0].clone();
        let order_id = PswapNote::try_from(&note).unwrap().order_id();
        Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), request))
            .await
            .unwrap();
        mock_rpc_api.prove_block();
        alice_client.sync_state().await.unwrap();
        order_id
    };

    let order_id_a = create_order(40, 20).await;
    let order_id_b = create_order(30, 15).await;

    // Same (note_type, offered_faucet, requested_faucet) → same tag for both orders.
    let asset_pair_tag = PswapNote::create_tag(
        NoteType::Public,
        &FungibleAsset::new(btc_faucet.id(), 40).unwrap(),
        &FungibleAsset::new(eth_faucet.id(), 20).unwrap(),
    );
    let pair_subscriptions = async |client: &MockClient<FilesystemKeyStore>| -> usize {
        client
            .get_note_tags()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| {
                r.tag == asset_pair_tag && matches!(r.source, NoteTagSource::Subscription(_))
            })
            .count()
    };
    assert_eq!(pair_subscriptions(&alice_client).await, 2, "both orders subscribe the tag");

    // Reclaim order A.
    let cancel_a = alice_client.build_pswap_cancel_by_order(order_id_a).await.unwrap();
    Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), cancel_a))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();
    assert_eq!(
        pair_subscriptions(&alice_client).await,
        1,
        "order B's subscription must survive order A's reclaim",
    );

    // Reclaim order B.
    let cancel_b = alice_client.build_pswap_cancel_by_order(order_id_b).await.unwrap();
    Box::pin(alice_client.submit_new_transaction(alice_wallet.id(), cancel_b))
        .await
        .unwrap();
    mock_rpc_api.prove_block();
    alice_client.sync_state().await.unwrap();
    assert_eq!(
        pair_subscriptions(&alice_client).await,
        0,
        "tag must be fully unsubscribed once the last order terminates",
    );
}

#[tokio::test]
async fn empty_storage_map() {
    let (mut client, _, keystore) = create_test_client().await;

    let storage_map = StorageMap::new();

    let component_code = CodeBuilder::default()
        .compile_component_code(
            "miden::testing::dummy_component",
            "pub proc dummy
                nop
            end",
        )
        .unwrap();
    let map_slot_name = StorageSlotName::new(EMPTY_STORAGE_MAP_SLOT_NAME).unwrap();
    let map_slot = StorageSlot::with_map(map_slot_name, storage_map);
    let component = AccountComponent::new(
        component_code,
        vec![map_slot],
        AccountComponentMetadata::new("miden::testing::dummy_component"),
    )
    .unwrap();

    let key_pair = AuthSecretKey::new_falcon512_poseidon2();
    let pub_key = key_pair.public_key();

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(BasicWallet)
        .with_component(component)
        .build_with_schema_commitment()
        .unwrap();

    let account_id = account.id();

    keystore.add_key(&key_pair, account_id).await.unwrap();

    client.add_account(&account, false).await.unwrap();

    let fetched_storage_commitment =
        client.account_reader(account_id).storage_commitment().await.unwrap();

    assert_eq!(account.storage().to_commitment(), fetched_storage_commitment);
}

const MAP_KEY: [Felt; 4] = [
    Felt::new_unchecked(42),
    Felt::new_unchecked(42),
    Felt::new_unchecked(42),
    Felt::new_unchecked(42),
];
const BUMP_MAP_SLOT_NAME: &str = "miden::testing::bump_map::map";
const EMPTY_STORAGE_MAP_SLOT_NAME: &str = "miden::testing::empty_storage_map::map";
// MASM code used by `storage_and_vault_proofs*` tests to mutate a storage map.
const BUMP_MAP_CODE: &str = r#"
                use miden::core::word

                const MAP_SLOT = word("miden::testing::bump_map::map")

                @account_procedure
                pub proc bump_map_item
                    # map key
                    push.{map_key}

                    # push slot_id_prefix, slot_id_suffix for the map slot
                    push.MAP_SLOT[0..2]

                    exec.::miden::protocol::active_account::get_map_item
                    add.1
                    push.{map_key}

                    # push slot_id_prefix, slot_id_suffix for the map slot
                    push.MAP_SLOT[0..2]
                    exec.::miden::protocol::native_account::set_map_item
                    dropw
                    # => [OLD_VALUE]

                    dupw

                    # push slot_id_prefix, slot_id_suffix for the map slot
                    push.MAP_SLOT[0..2]

                    # Set a new item each time as the value keeps changing
                    exec.::miden::protocol::native_account::set_map_item
                    dropw dropw
                end"#;

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn storage_and_vault_proofs() {
    let (mut client, mock_rpc_api, keystore) = create_test_client().await;

    // Create an account that will accept assets (basic wallet) but also that has a storage map that
    // can be updated.
    let mut storage_map = StorageMap::new();
    storage_map
        .insert(
            StorageMapKey::new(MAP_KEY.into()),
            [Felt::from(0u32), Felt::from(0u32), Felt::from(0u32), Felt::from(1u32)].into(),
        )
        .unwrap();

    let bump_component_code = CodeBuilder::default()
        .compile_component_code(
            "miden::testing::bump_map_component",
            BUMP_MAP_CODE.replace("{map_key}", &Word::from(MAP_KEY).to_hex()),
        )
        .unwrap();
    let bump_map_slot_name = StorageSlotName::new(BUMP_MAP_SLOT_NAME).unwrap();
    let bump_map_slot = StorageSlot::with_map(bump_map_slot_name.clone(), storage_map);
    let bump_item_component = AccountComponent::new(
        bump_component_code,
        vec![bump_map_slot],
        AccountComponentMetadata::new("miden::testing::bump_map_component"),
    )
    .unwrap();

    // Build script that bumps the storage map item and adds a new one each time.
    let tx_script = CodeBuilder::new()
        .with_linked_module(
            "external_contract::bump_item_contract",
            BUMP_MAP_CODE.replace("{map_key}", &Word::from(MAP_KEY).to_hex()),
        )
        .unwrap()
        .compile_tx_script(
            "use external_contract::bump_item_contract
            @transaction_script
            pub proc main
                call.bump_item_contract::bump_map_item
            end",
        )
        .unwrap();

    let key_pair = AuthSecretKey::new_falcon512_poseidon2();
    let pub_key = key_pair.public_key();

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(BasicWallet)
        .with_component(bump_item_component)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await.unwrap();

    let account_id = account.id();

    // Add assets and modify storage map multiple times
    for _ in 0..5 {
        let faucet_account =
            insert_new_fungible_faucet(&mut client, AccountType::Public, &keystore)
                .await
                .unwrap();

        let faucet_account_id = faucet_account.id();

        mint_and_consume(&mut client, account_id, faucet_account_id, NoteType::Private).await;
        mock_rpc_api.prove_block();
        client.sync_state().await.unwrap();

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(tx_script.clone())
            .build()
            .unwrap();
        Box::pin(client.submit_new_transaction(account_id, tx_request)).await.unwrap();
        mock_rpc_api.prove_block();
        client.sync_state().await.unwrap();

        // Check that retrieved vault and storage match with the account.
        let account_reader = client.account_reader(account_id);
        let account_storage_commitment = account_reader.storage_commitment().await.unwrap();
        let account_vault_root = account_reader.vault_root().await.unwrap();

        let storage = client
            .test_store()
            .get_account_storage(account_id, AccountStorageFilter::All)
            .await
            .unwrap();
        let vault = client.test_store().get_account_vault(account_id).await.unwrap();

        assert_eq!(account_storage_commitment, storage.to_commitment());
        assert_eq!(account_vault_root, vault.root());

        // Check that specific asset proof matches the one in the vault
        let asset_id = AssetId::new_fungible(faucet_account_id);
        let (asset, witness) = client
            .test_store()
            .get_account_asset(account_id, asset_id)
            .await
            .unwrap()
            .unwrap();

        let expected_witness = vault.open(asset.id());
        assert_eq!(witness, expected_witness);

        // Check that specific map item proof matches the one in the storage
        let (value, proof) = client
            .test_store()
            .get_account_map_item(
                account_id,
                bump_map_slot_name.clone(),
                StorageMapKey::new(MAP_KEY.into()),
            )
            .await
            .unwrap();

        let map_slot = storage
            .slots()
            .iter()
            .find(|slot| slot.name() == &bump_map_slot_name)
            .expect("storage should contain bump map slot");
        let StorageSlotContent::Map(map) = map_slot.content() else {
            panic!("Expected bump map slot content to be a map");
        };

        assert_eq!(value, map.get(&StorageMapKey::new(MAP_KEY.into())));
        assert_eq!(proof, map.open(&StorageMapKey::new(MAP_KEY.into())));
    }
}

#[tokio::test]
async fn account_addresses_basic_wallet() {
    // generate test client with a random store name
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )),
    );

    client.add_account(&account, false).await.unwrap();
    let addresses = client.account_reader(account.id()).addresses().await.unwrap();

    let unspecified_default_address = Address::new(account.id());
    assert!(addresses.contains(&unspecified_default_address));

    // Even when the account has a basic wallet, the address list should not contain it by default
    let routing_params = RoutingParameters::new(AddressInterface::BasicWallet);
    let basic_wallet_address = Address::new(account.id()).with_routing_parameters(routing_params);
    assert!(!addresses.contains(&basic_wallet_address));
}

#[tokio::test]
async fn account_addresses_non_basic_wallet() {
    // generate test client with a random store name
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock_non_fungible_faucet(ACCOUNT_ID_PUBLIC_NON_FUNGIBLE_FAUCET);

    client.add_account(&account, false).await.unwrap();
    let addresses = client.account_reader(account.id()).addresses().await.unwrap();

    let unspecified_default_address = Address::new(account.id());
    assert!(addresses.contains(&unspecified_default_address));

    let routing_params = RoutingParameters::new(AddressInterface::BasicWallet);
    let basic_wallet_address = Address::new(account.id()).with_routing_parameters(routing_params);
    assert!(!addresses.contains(&basic_wallet_address));
}

#[tokio::test]
async fn account_add_address_after_creation() {
    // generate test client with a random store name
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    let account = Account::mock(
        ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_2,
        AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )),
    );

    client.add_account(&account, false).await.unwrap();

    let default_address = Address::new(account.id());

    // The address cannot be added again as it is already present after account creation
    assert!(client.add_address(default_address.clone(), account.id()).await.is_err());

    // An address with different routing parameters can be added
    let routing_params = RoutingParameters::new(AddressInterface::BasicWallet);
    let basic_wallet_address = Address::new(account.id()).with_routing_parameters(routing_params);
    assert!(client.add_address(basic_wallet_address.clone(), account.id()).await.is_ok());

    // We can remove the default address and the note tag is still present
    assert!(client.remove_address(default_address.clone(), account.id()).await.is_ok());
    let derived_note_tag = default_address.to_note_tag();
    let note_tag_record = NoteTagRecord::with_account_source(derived_note_tag, account.id());
    let note_tags = client.get_note_tags().await.unwrap();
    assert!(note_tags.contains(&note_tag_record));

    // If we remove all addresses, note tag should be removed
    assert!(client.remove_address(basic_wallet_address.clone(), account.id()).await.is_ok());
    let note_tags = client.get_note_tags().await.unwrap();
    assert!(!note_tags.contains(&note_tag_record));

    // Then add it again
    assert!(client.add_address(default_address, account.id()).await.is_ok());

    // Derived note tag should now be available
    let note_tags = client.get_note_tags().await.unwrap();
    assert!(note_tags.contains(&note_tag_record));
}

#[tokio::test]
async fn import_watched_account_by_id_rejects_already_tracked_native_account() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();
    let account_id = account.id();
    let rpc_api = MockRpcApi::new(mock_chain_builder.build().unwrap());
    let arc_rpc_api = Arc::new(rpc_api);
    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();
    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());
    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();
    let mut client = ClientBuilder::new()
        .rpc(arc_rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    client.add_account(&account, false).await.unwrap();

    let default_note_tag_record =
        NoteTagRecord::with_account_source(Address::new(account_id).to_note_tag(), account_id);
    let routing_params = RoutingParameters::new(AddressInterface::BasicWallet)
        .with_note_tag_len(NoteTag::MAX_ACCOUNT_TARGET_TAG_LENGTH)
        .unwrap();
    let extra_address = Address::new(account_id).with_routing_parameters(routing_params);
    let extra_address_note_tag_record =
        NoteTagRecord::with_account_source(extra_address.to_note_tag(), account_id);

    client.add_address(extra_address, account_id).await.unwrap();

    let note_tags = client.get_note_tags().await.unwrap();
    assert!(note_tags.contains(&default_note_tag_record));
    assert!(note_tags.contains(&extra_address_note_tag_record));

    let err = client
        .import_watched_account_by_id(account_id)
        .await
        .expect_err("watched import must reject already-tracked native account");
    assert!(matches!(err, ClientError::AccountWatchedMismatch(id) if id == account_id));

    // Native tags must still be there and the account must still be native.
    let note_tags = client.get_note_tags().await.unwrap();
    assert!(note_tags.contains(&default_note_tag_record));
    assert!(note_tags.contains(&extra_address_note_tag_record));
    let account_record = client.test_store().get_account(account_id).await.unwrap().unwrap();
    assert!(!account_record.is_watched());
}

// TODO: fix - blocked by an upstream miden-standards bug (0.16.0-alpha.2). Creating the zero-asset
// output note from a basic wallet hits `send_notes_script.rs::move_asset_to_note_body`, whose
// `pad(21)->pad(16)` stack reduction only runs inside the per-asset loop; with no assets the tx
// script returns at stack depth 21 and the VM rejects it with `InvalidStackDepthOnReturn`.
// Re-enable once the standards send-notes script handles zero-asset notes.
#[tokio::test]
async fn consume_note_with_custom_script() {
    let (mut client, mock_rpc_api, keystore) = create_test_client().await;

    let (sender_account, receiver_account, faucet_account) = setup_two_wallets_and_faucet(
        &mut client,
        AccountType::Private,
        &keystore,
        RPO_FALCON_SCHEME_ID,
    )
    .await
    .unwrap();

    let sender_id = sender_account.id();
    let receiver_id = receiver_account.id();
    let faucet_id = faucet_account.id();

    mint_and_consume(&mut client, sender_id, faucet_id, NoteType::Private).await;
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    let custom_note_script = "
        use miden::core::sys
        @note_script
        pub proc main
            exec.sys::truncate_stack
        end
    ";
    let note_script = client.code_builder().compile_note_script(custom_note_script).unwrap();

    let note_storage = NoteStorage::new(vec![]).unwrap();
    let serial_num = client.rng().draw_word();
    let note_metadata = PartialNoteMetadata::new(sender_id, NoteType::Private)
        .with_tag(NoteTag::with_account_target(receiver_id));
    let note_assets = NoteAssets::new(vec![]).unwrap();
    let note_recipient = NoteRecipient::new(serial_num, note_script.clone(), note_storage);
    let custom_note = Note::new(note_assets, note_metadata, note_recipient);

    // At this point, the note script should no be stored locally
    assert!(client.test_store().get_note_script(note_script.root().into()).await.is_err());

    let tx_request = TransactionRequestBuilder::new()
        .own_output_notes(vec![custom_note.clone()])
        .build()
        .unwrap();
    let _tx_id = Box::pin(client.submit_new_transaction(sender_id, tx_request)).await.unwrap();
    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();

    // At this point, the note script should be stored locally
    let stored_script =
        client.test_store().get_note_script(note_script.root().into()).await.unwrap();
    assert_eq!(stored_script.root().to_hex(), note_script.root().to_hex());

    // Consume note
    let transaction_request = TransactionRequestBuilder::new()
        .build_consume_notes(vec![custom_note.clone()])
        .unwrap();

    // The transaction should be submitted successfully
    let _transaction = Box::pin(client.submit_new_transaction(receiver_id, transaction_request))
        .await
        .unwrap();

    mock_rpc_api.prove_block();
    client.sync_state().await.unwrap();
}

// PAGINATION TESTS
// ================================================================================================

#[tokio::test]
async fn sync_storage_maps_pagination() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let _mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    for _ in 0..12 {
        mock_chain.prove_next_block().unwrap();
    }

    let rpc_api = MockRpcApi::new(mock_chain);
    let chain_tip = rpc_api.get_chain_tip_block_num();

    assert!(chain_tip.as_u32() >= 12);

    let account_id = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
    let result = rpc_api.sync_storage_maps(0.into(), chain_tip, account_id).await.unwrap();

    // Verify we got a response covering the full range
    assert_eq!(result.chain_tip, chain_tip);
    assert_eq!(result.block_number, chain_tip);
}

/// Tests that `sync_account_vault` correctly accumulates data across multiple pagination pages.
#[tokio::test]
async fn sync_account_vault_pagination() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let _mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    for _ in 0..12 {
        mock_chain.prove_next_block().unwrap();
    }

    let rpc_api = MockRpcApi::new(mock_chain);
    let chain_tip = rpc_api.get_chain_tip_block_num();

    // Chain should have at least 12 blocks
    assert!(chain_tip.as_u32() >= 12);

    // Sync from block 0 to chain tip - this should require multiple pagination calls internally
    let account_id = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
    let result = rpc_api.sync_account_vault(0.into(), chain_tip, account_id).await.unwrap();

    // Verify we got a response covering the full range
    assert_eq!(result.chain_tip, chain_tip);
    assert_eq!(result.block_number, chain_tip);
}

/// Tests `sync_storage_maps` pagination with a specific `block_to` parameter.
#[tokio::test]
async fn sync_storage_maps_pagination_with_block_to() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let _mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    for _ in 0..15 {
        mock_chain.prove_next_block().unwrap();
    }

    let rpc_api = MockRpcApi::new(mock_chain);

    let account_id = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
    let result = rpc_api.sync_storage_maps(0.into(), 10.into(), account_id).await.unwrap();

    // Verifies we stopped at block 10, not chain tip
    assert_eq!(result.block_number.as_u32(), 10);
}

/// Tests `sync_account_vault` pagination with a specific `block_to` parameter.
#[tokio::test]
async fn sync_account_vault_pagination_with_block_to() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let _mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    for _ in 0..15 {
        mock_chain.prove_next_block().unwrap();
    }

    let rpc_api = MockRpcApi::new(mock_chain);

    let account_id = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
    let result = rpc_api.sync_account_vault(0.into(), 10.into(), account_id).await.unwrap();

    // Verify we stopped at block 10, not chain tip
    assert_eq!(result.block_number.as_u32(), 10);
}

/// Tests that pagination works correctly when starting from a non-zero block.
#[tokio::test]
async fn sync_storage_maps_pagination_from_middle() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let _mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    // Create 15 blocks
    for _ in 0..15 {
        mock_chain.prove_next_block().unwrap();
    }

    let rpc_api = MockRpcApi::new(mock_chain);
    let chain_tip = rpc_api.get_chain_tip_block_num();

    let account_id = AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
    let result = rpc_api.sync_storage_maps(7.into(), chain_tip, account_id).await.unwrap();

    assert_eq!(result.chain_tip, chain_tip);
    assert_eq!(result.block_number, chain_tip);
}

// PRIVATE NOTE ATTACHMENT SYNC TESTS
// ================================================================================================

/// A private note carries its [`NoteAttachments`] on-chain: they feed the metadata commitment and
/// thus the note ID. This test verifies that when such a note commits during a regular state sync,
/// the client fetches the attachments via `GetNotesById` and stores them on the resulting
/// [`InputNoteRecord`], so the note can be reconstructed with the same ID it has on-chain (and is
/// therefore consumable).
#[tokio::test]
async fn sync_stores_private_note_attachments() {
    // 1. Build a mock chain with a sender and a public target account (the attachment target must
    //    be public).
    let mut mock_chain_builder = MockChainBuilder::new();
    let faucet_id = AccountId::dummy(
        [7u8; 15],
        AccountIdVersion::Version1,
        AccountType::Public,
        AssetCallbackFlag::Disabled,
    );
    let note_asset = FungibleAsset::new(faucet_id, 100).unwrap();
    let sender = mock_chain_builder
        .add_existing_mock_account_with_assets(miden_testing::Auth::IncrNonce, [note_asset.into()])
        .unwrap();
    let target = mock_chain_builder.add_existing_wallet(miden_testing::Auth::IncrNonce).unwrap();

    // 2. Build a PRIVATE P2ID note carrying a NetworkAccountTarget attachment.
    let ntx_target = NetworkAccountTarget::new(target.id(), NoteExecutionHint::Always).unwrap();
    let attachments = NoteAttachments::new(vec![ntx_target.into()]).unwrap();
    let mut note_rng = RandomCoin::new([1, 2, 3, 4].map(Felt::new_unchecked).into());
    let private_note = P2idNote::builder()
        .sender(sender.id())
        .target(target.id())
        .asset(note_asset)
        .note_type(NoteType::Private)
        .attachments(attachments.clone().into_vec())
        .generate_serial_number(&mut note_rng)
        .build()
        .unwrap()
        .into();

    // Declare the note as a spawn note (not yet committed) and build the chain at genesis.
    let spawn_note =
        mock_chain_builder.add_spawn_note(std::slice::from_ref(&private_note)).unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    // 3. Commit the private note at block 1, then advance a few blocks.
    let tx = Box::pin(
        mock_chain
            .build_tx_context(TxContextInput::AccountId(sender.id()), &[], &[spawn_note])
            .unwrap()
            .extend_expected_output_notes(vec![RawOutputNote::Full(private_note.clone())])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();
    mock_chain.prove_next_block().unwrap();
    for _ in 0..3 {
        mock_chain.prove_next_block().unwrap();
    }

    // 4. Build a client backed by this chain. A fixed node returns private-note attachments via
    //    `get_notes_by_id`, but the MockChain stores private notes without their attachment
    //    content, so register them on the mock RPC explicitly.
    let rpc_api = Arc::new(MockRpcApi::new(mock_chain));
    rpc_api.register_private_note_attachments(private_note.id(), attachments.clone());

    let rng =
        RandomCoin::new(rand::random::<[u64; 4]>().map(|v| Felt::new_unchecked(v >> 1)).into());
    let keystore = FilesystemKeyStore::new(std::env::temp_dir()).unwrap();
    let mut client = ClientBuilder::new()
        .rpc(rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    // 5. Track the note as an expected input note (no metadata, empty attachments) and register its
    //    tag so the chain sync sees the note's block. The client has not synced past block 1, so
    //    the import leaves the note in the Expected state rather than committing it.
    let note_tag = private_note.metadata().tag();
    client.add_note_tag(note_tag).await.unwrap();
    client
        .import_notes(&[NoteFile::ExpectedNote {
            details: private_note.clone().into(),
            sync_hint: NoteSyncHint::new(BlockNumber::from(0u32), note_tag),
        }])
        .await
        .unwrap();

    let expected = client
        .get_input_notes(NoteFilter::DetailsCommitments(vec![private_note.details_commitment()]))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(
        expected.attachments().is_empty(),
        "imported expected note should start with empty attachments"
    );

    // 6. Sync: the note commits via the regular note-state sync path, which fetches the attachments
    //    and stores them on the record.
    client.sync_state().await.unwrap();

    // 7. The committed record should carry the original attachments and reconstruct to the same
    //    note ID as the on-chain note.
    let committed = client
        .get_input_notes(NoteFilter::Committed)
        .await
        .unwrap()
        .into_iter()
        .find(|n| n.id() == Some(private_note.id()))
        .expect("private note should be committed after sync");

    assert_eq!(
        committed.attachments(),
        &attachments,
        "sync should store the private note's attachments on the record"
    );

    let reconstructed: Note = (&committed).try_into().unwrap();
    assert_eq!(
        reconstructed.id(),
        private_note.id(),
        "reconstructed note must match the on-chain note ID (attachments feed the ID)"
    );
}

// LARGE PUBLIC ACCOUNT SYNC TESTS
// ================================================================================================

/// Tests that syncing a public account with a large storage map works correctly.
/// The account is synced via full-state replacement after `get_account_details`
/// internally handles the oversized storage maps.
#[tokio::test]
async fn sync_large_public_account() {
    // 1. Create a public account with a large storage map and many vault assets.
    let map_slot = StorageSlot::with_map(
        StorageSlotName::new("test::large_map").unwrap(),
        StorageMap::with_entries(
            (1..=NUM_STORAGE_MAP_ENTRIES_LARGE_ACCOUNT)
                .map(|i| {
                    let w = Word::from([
                        Felt::new_unchecked(i),
                        Felt::from(0u32),
                        Felt::from(0u32),
                        Felt::from(0u32),
                    ]);
                    (StorageMapKey::new(w), w)
                })
                .collect::<Vec<_>>(),
        )
        .unwrap(),
    );

    let mut builder = MockChainBuilder::new();

    // Create faucets so we can give the account enough assets to exceed the oversize threshold.
    let faucets: Vec<Account> = (0..NUM_FAUCETS_LARGE_ACCOUNT)
        .map(|i| {
            // TokenSymbol requires uppercase ASCII letters only.
            let symbol = format!("TK{}", (b'A' + u8::try_from(i).unwrap()) as char);
            builder
                .add_existing_basic_faucet(miden_testing::Auth::IncrNonce, &symbol, 1_000_000, None)
                .unwrap()
        })
        .collect();

    let assets: Vec<Asset> = faucets
        .iter()
        .map(|faucet| Asset::Fungible(FungibleAsset::new(faucet.id(), 100).unwrap()))
        .collect();

    let mock_account = builder
        .add_existing_mock_account_with_storage_and_assets(
            miden_testing::Auth::IncrNonce,
            [map_slot],
            assets,
        )
        .unwrap();
    let original_account = mock_account.clone();
    let mut mock_chain = builder.build().unwrap();

    // 2. Execute a transaction that increments the account's nonce.
    // This changes the on-chain commitment so sync detects a mismatch.
    let tx = Box::pin(
        mock_chain
            .build_tx_context(TxContextInput::AccountId(mock_account.id()), &[], &[])
            .unwrap()
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();
    mock_chain.prove_next_block().unwrap();

    // 3. Create MockRpcApi with a low oversize threshold so both the storage map
    // and vault trigger the `too_many_entries` / `too_many_assets` flags.
    let rpc_api = MockRpcApi::new(mock_chain).with_oversize_threshold(OVERSIZE_THRESHOLD);
    let arc_rpc_api = Arc::new(rpc_api.clone());

    // 4. Build a client and add the ORIGINAL (pre-tx) account.
    // The pre-tx commitment differs from on-chain, which triggers sync.
    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();
    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());

    let keystore_path = temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path).unwrap();

    let mut client = ClientBuilder::new()
        .rpc(arc_rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    client.add_account(&original_account, false).await.unwrap();

    // 5. Sync — the client detects a commitment mismatch, fetches full account state.
    client.sync_state().await.unwrap();

    // 6. Verify the synced account matches the on-chain state.
    let synced_account: Account = client.get_account(mock_account.id()).await.unwrap().unwrap();
    let on_chain_account =
        rpc_api.mock_chain.read().committed_account(mock_account.id()).unwrap().clone();

    assert_eq!(
        synced_account.to_commitment(),
        on_chain_account.to_commitment(),
        "client should have the updated account state after sync"
    );

    // Verify the storage map entries are preserved.
    let map_name = StorageSlotName::new("test::large_map").unwrap();
    let map_slot = synced_account
        .storage()
        .slots()
        .iter()
        .find(|s| *s.name() == map_name)
        .expect("large map slot should exist after sync");
    let StorageSlotContent::Map(map) = map_slot.content() else {
        panic!("expected map slot content");
    };
    assert_eq!(
        map.entries().count(),
        usize::try_from(NUM_STORAGE_MAP_ENTRIES_LARGE_ACCOUNT).unwrap(),
        "all map entries should be preserved after sync"
    );

    // Verify the vault assets are preserved.
    let synced_assets: Vec<Asset> = synced_account.vault().assets().collect();
    assert_eq!(
        synced_assets.len(),
        usize::try_from(NUM_FAUCETS_LARGE_ACCOUNT).unwrap(),
        "all vault assets should be preserved after sync"
    );
}

#[tokio::test]
async fn prepare_offline_bootstrap_inserts_mock_chain_genesis() {
    use miden_protocol::block::account_tree::AccountTree;
    use miden_protocol::crypto::merkle::smt::Smt;
    use miden_protocol::transaction::TransactionKernel;

    let mut rng_seed = rand::rng();
    let coin_seed: [u64; 4] = rng_seed.random();
    let rng = RandomCoin::new(coin_seed.map(Felt::new_unchecked).into());

    let reference_rpc = MockRpcApi::default();
    let (expected_genesis, _) = reference_rpc
        .get_block_header_by_number(Some(BlockNumber::GENESIS), false)
        .await
        .unwrap();

    let keystore_path = temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path).unwrap();

    let mut client = ClientBuilder::new()
        .rpc(Arc::new(MockRpcApi::default()))
        .sqlite_store(create_test_store_path())
        .rng(Box::new(rng))
        .authenticator(Arc::new(keystore))
        .build()
        .await
        .unwrap();

    client.prepare_offline_bootstrap().await.unwrap();

    let (stored_genesis, _) = client
        .get_block_header_by_num(BlockNumber::GENESIS)
        .await
        .unwrap()
        .expect("genesis should be stored after offline bootstrap");

    assert_eq!(stored_genesis.block_num(), BlockNumber::GENESIS);
    assert_eq!(stored_genesis.account_root(), expected_genesis.account_root());
    assert_eq!(stored_genesis.tx_kernel_commitment(), expected_genesis.tx_kernel_commitment());
    assert_eq!(stored_genesis.account_root(), AccountTree::<Smt>::default().root());
    assert_eq!(stored_genesis.tx_kernel_commitment(), TransactionKernel.to_commitment());
}

// HELPERS
// ================================================================================================

pub async fn create_test_client() -> (MockClient<FilesystemKeyStore>, MockRpcApi, FilesystemKeyStore)
{
    let (builder, rpc_api, keystore) = Box::pin(create_test_client_builder()).await;
    let mut client = builder.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    (client, rpc_api, keystore)
}

pub async fn create_test_client_builder()
-> (ClientBuilder<FilesystemKeyStore>, MockRpcApi, FilesystemKeyStore) {
    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();

    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());

    let keystore_path = temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path).unwrap();

    let rpc_api = MockRpcApi::new(Box::pin(create_prebuilt_mock_chain()).await);
    let arc_rpc_api = Arc::new(rpc_api.clone());

    let builder = ClientBuilder::new()
        .rpc(arc_rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore.clone()))
        .tx_discard_delta(None);

    (builder, rpc_api, keystore)
}

pub async fn create_prebuilt_mock_chain() -> MockChain {
    let mut mock_chain_builder = MockChainBuilder::new();
    let mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();

    let note_first = NoteBuilder::new(
        mock_account.id(),
        RandomCoin::new([0, 0, 0, 0].map(Felt::new_unchecked).into()),
    )
    .note_type(NoteType::Public)
    .tag(NoteTag::new(0).into())
    .build()
    .unwrap();

    let note_second = NoteBuilder::new(
        mock_account.id(),
        RandomCoin::new([0, 0, 0, 1].map(Felt::new_unchecked).into()),
    )
    .note_type(NoteType::Public)
    .tag(NoteTag::new(0).into())
    .build()
    .unwrap();
    let spawn_note_1 =
        mock_chain_builder.add_spawn_note(std::slice::from_ref(&note_first)).unwrap();
    let spawn_note_2 =
        mock_chain_builder.add_spawn_note(std::slice::from_ref(&note_second)).unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    // Block 1: Create first note
    let tx = Box::pin(
        mock_chain
            .build_tx_context(TxContextInput::AccountId(mock_account.id()), &[], &[spawn_note_1])
            .unwrap()
            .extend_expected_output_notes(vec![RawOutputNote::Full(note_first)])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();
    mock_chain.prove_next_block().unwrap();

    // Block 2
    mock_chain.prove_next_block().unwrap();

    // Block 3
    mock_chain.prove_next_block().unwrap();

    // Block 4: Create second note

    let tx = Box::pin(
        mock_chain
            .build_tx_context(mock_account.id(), &[], &[spawn_note_2])
            .unwrap()
            .extend_expected_output_notes(vec![RawOutputNote::Full(note_second.clone())])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();

    mock_chain.prove_next_block().unwrap();

    let transaction = Box::pin(
        mock_chain
            .build_tx_context(mock_account.id(), &[], &[note_second])
            .unwrap()
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();

    // Block 5: Consume (nullify) second note
    mock_chain.add_pending_executed_transaction(&transaction).unwrap();
    mock_chain.prove_next_block().unwrap();

    mock_chain
}

async fn insert_new_wallet(
    client: &mut TestClient,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
) -> Result<Account, ClientError> {
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let pub_key = key_pair.public_key();

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let account = AccountBuilder::new(init_seed)
        .account_type(visibility)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await?;

    Ok(account)
}

async fn insert_new_ecdsa_wallet(
    client: &mut TestClient,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
) -> Result<Account, ClientError> {
    let init_seed = [0u8; 32];
    let mut rng = StdRng::from_seed(init_seed);

    let key_pair = AuthSecretKey::new_ecdsa_k256_keccak_with_rng(&mut rng);
    let pub_key = key_pair.public_key();

    let account = AccountBuilder::new(init_seed)
        .account_type(visibility)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::EcdsaK256Keccak,
        )))
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await?;

    Ok(account)
}

async fn insert_new_fungible_faucet(
    client: &mut TestClient,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
) -> Result<Account, ClientError> {
    let key_pair = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
    let pub_key = key_pair.public_key();

    // we need to use an initial seed to create the wallet account
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let symbol = TokenSymbol::new("TEST").unwrap();
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let max_supply = 9_999_999_u64;
    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(10)
        .max_supply(AssetAmount::new(max_supply).unwrap())
        .build()
        .unwrap();
    // Only mint/burn policies — see test_utils/common.rs::insert_new_fungible_faucet for the
    // reason transfer policies are intentionally omitted.
    let policy_manager = TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::allow_all())
        .active_burn_policy(BurnPolicy::allow_all())
        .build();

    let account = AccountBuilder::new(init_seed)
        .account_type(visibility)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(faucet)
        .with_components(policy_manager)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await?;
    Ok(account)
}

async fn insert_new_ecdsa_fungible_faucet(
    client: &mut TestClient,
    visibility: AccountType,
    keystore: &FilesystemKeyStore,
) -> Result<Account, ClientError> {
    let init_seed = [0u8; 32];
    let mut rng = StdRng::from_seed(init_seed);

    let key_pair = AuthSecretKey::new_ecdsa_k256_keccak_with_rng(&mut rng);
    let pub_key = key_pair.public_key();

    // we need to use an initial seed to create the wallet account
    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let symbol = TokenSymbol::new("TEST").unwrap();
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let max_supply = 9_999_999_u64;
    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(10)
        .max_supply(AssetAmount::new(max_supply).unwrap())
        .build()
        .unwrap();
    // Only mint/burn policies — see test_utils/common.rs::insert_new_fungible_faucet for the
    // reason transfer policies are intentionally omitted.
    let policy_manager = TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::allow_all())
        .active_burn_policy(BurnPolicy::allow_all())
        .build();

    let account = AccountBuilder::new(init_seed)
        .account_type(visibility)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::EcdsaK256Keccak,
        )))
        .with_component(faucet)
        .with_components(policy_manager)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await?;
    Ok(account)
}

#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn storage_and_vault_proofs_ecdsa() {
    let (mut client, mock_rpc_api, keystore) = create_test_client().await;

    // Create an account that will accept assets (basic wallet) but also that has a storage map that
    // can be updated.
    //
    // Same setup as `storage_and_vault_proofs`, but using ECDSA auth instead of RPO Falcon.
    // The storage map is still updated via named-slot access in `BUMP_MAP_CODE`.
    let mut storage_map = StorageMap::new();
    storage_map
        .insert(
            StorageMapKey::new(MAP_KEY.into()),
            [Felt::from(0u32), Felt::from(0u32), Felt::from(0u32), Felt::from(1u32)].into(),
        )
        .unwrap();

    let bump_component_code = CodeBuilder::default()
        .compile_component_code(
            "miden::testing::bump_map_component",
            BUMP_MAP_CODE.replace("{map_key}", &Word::from(MAP_KEY).to_hex()),
        )
        .unwrap();
    let bump_map_slot_name = StorageSlotName::new(BUMP_MAP_SLOT_NAME).unwrap();
    let bump_map_slot = StorageSlot::with_map(bump_map_slot_name.clone(), storage_map);
    let bump_item_component = AccountComponent::new(
        bump_component_code,
        vec![bump_map_slot],
        AccountComponentMetadata::new("miden::testing::bump_map_component"),
    )
    .unwrap();

    // Build script that bumps the storage map item and adds a new one each time.
    let tx_script = CodeBuilder::new()
        .with_linked_module(
            "external_contract::bump_item_contract",
            BUMP_MAP_CODE.replace("{map_key}", &Word::from(MAP_KEY).to_hex()),
        )
        .unwrap()
        .compile_tx_script(
            "use external_contract::bump_item_contract
            @transaction_script
            pub proc main
                call.bump_item_contract::bump_map_item
            end",
        )
        .unwrap();

    let key_pair = AuthSecretKey::new_ecdsa_k256_keccak();
    let pub_key = key_pair.public_key();

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let account = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            pub_key.to_commitment(),
            AuthSchemeId::EcdsaK256Keccak,
        )))
        .with_component(BasicWallet)
        .with_component(bump_item_component)
        .build_with_schema_commitment()
        .unwrap();

    keystore.add_key(&key_pair, account.id()).await.unwrap();

    client.add_account(&account, false).await.unwrap();

    let account_id = account.id();

    // Add assets and modify storage map multiple times
    for _ in 0..5 {
        let faucet_account =
            insert_new_ecdsa_fungible_faucet(&mut client, AccountType::Public, &keystore)
                .await
                .unwrap();

        let faucet_account_id = faucet_account.id();

        mint_and_consume(&mut client, account_id, faucet_account_id, NoteType::Private).await;
        mock_rpc_api.prove_block();
        client.sync_state().await.unwrap();

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(tx_script.clone())
            .build()
            .unwrap();
        Box::pin(client.submit_new_transaction(account_id, tx_request)).await.unwrap();
        mock_rpc_api.prove_block();
        client.sync_state().await.unwrap();

        // Check that retrieved vault and storage match with the account.
        let account_reader = client.account_reader(account_id);
        let account_storage_commitment = account_reader.storage_commitment().await.unwrap();
        let account_vault_root = account_reader.vault_root().await.unwrap();

        let storage = client
            .test_store()
            .get_account_storage(account_id, AccountStorageFilter::All)
            .await
            .unwrap();
        let vault = client.test_store().get_account_vault(account_id).await.unwrap();

        assert_eq!(account_storage_commitment, storage.to_commitment());
        assert_eq!(account_vault_root, vault.root());

        // Check that specific asset proof matches the one in the vault
        let asset_id = AssetId::new_fungible(faucet_account_id);
        let (asset, witness) = client
            .test_store()
            .get_account_asset(account_id, asset_id)
            .await
            .unwrap()
            .unwrap();

        let expected_witness = vault.open(asset.id());
        assert_eq!(witness, expected_witness);

        // Check that specific map item proof matches the one in the storage
        let (value, proof) = client
            .test_store()
            .get_account_map_item(
                account_id,
                bump_map_slot_name.clone(),
                StorageMapKey::new(MAP_KEY.into()),
            )
            .await
            .unwrap();

        let map_slot = storage
            .slots()
            .iter()
            .find(|slot| slot.name() == &bump_map_slot_name)
            .expect("storage should contain bump map slot");
        let StorageSlotContent::Map(map) = map_slot.content() else {
            panic!("Expected bump map slot content to be a map");
        };

        assert_eq!(value, map.get(&StorageMapKey::new(MAP_KEY.into())));
        assert_eq!(proof, map.open(&StorageMapKey::new(MAP_KEY.into())));
    }
}

#[tokio::test]
async fn execute_transaction_fails_for_watched_account() {
    let (mut client, _rpc_api, _) = Box::pin(create_test_client()).await;

    // Build a faucet locally and insert it directly as watched via the store. Bypasses the
    // public `add_account`/`import_watched_account_by_id` paths so we don't need a mock RPC
    // round-trip.
    let key_pair = AuthSecretKey::new_falcon512_poseidon2();
    let auth_component = AuthSingleSig::new(Approver::new(
        key_pair.public_key().to_commitment(),
        AuthSchemeId::Falcon512Poseidon2,
    ));

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let symbol = TokenSymbol::new("WTCH").unwrap();
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let max_supply = 9_999_999_u64;
    let token = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(10)
        .max_supply(AssetAmount::new(max_supply).unwrap())
        .build()
        .unwrap();
    let policy_manager = TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::allow_all())
        .active_burn_policy(BurnPolicy::allow_all())
        .build();
    let faucet = AccountBuilder::new(init_seed)
        .account_type(AccountType::Public)
        .with_auth_component(auth_component)
        .with_component(token)
        .with_components(policy_manager)
        .build_with_schema_commitment()
        .unwrap();
    let faucet_id = faucet.id();
    let address = Address::new(faucet_id);

    client
        .test_store()
        .insert_account(&faucet, address, ClientAccountType::Watched)
        .await
        .expect("watched account should insert via the store");

    let target_account_id = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET_1).unwrap();
    let tx_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet_id, 1u64).unwrap(),
            target_account_id,
            miden_protocol::note::NoteType::Private,
            client.rng(),
        )
        .unwrap();

    let result = Box::pin(client.execute_transaction(faucet_id, tx_request)).await;

    match result {
        Err(ClientError::AccountIsWatched(id)) => assert_eq!(id, faucet_id),
        other => panic!("expected AccountIsWatched, got {other:?}"),
    }
}

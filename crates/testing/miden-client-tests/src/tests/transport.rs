use std::collections::{BTreeMap, BTreeSet};
use std::env::temp_dir;
use std::sync::Arc;

use miden_client::account::{Account, AccountType};
use miden_client::address::{Address, AddressInterface, RoutingParameters};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::{
    NetworkAccountTarget,
    Note,
    NoteDetails,
    NoteExecutionHint,
    NoteFile,
    NoteInclusionProof,
    NoteSyncHint,
    NoteTag,
    NoteType,
    PartialNoteMetadata,
};
use miden_client::note_transport::{
    NOTE_TRANSPORT_CURSORS_KEY,
    NoteTransportClient,
    NoteTransportCursor,
    NoteTransportError,
};
use miden_client::store::{NoteFilter, SettingScope};
use miden_client::sync::{NoteTagRecord, NoteTagSource};
use miden_client::testing::common::{TestClient, create_test_store_path};
use miden_client::testing::mock::{MockClient, MockRpcApi};
use miden_client::testing::note_transport::{
    FaultyNoteTransportApi,
    MockNoteTransportApi,
    MockNoteTransportNode,
};
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::utils::RwLock;
use miden_client::{ClientError, Deserializable};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::Word;
use miden_protocol::account::{
    AccountId,
    AccountIdVersion,
    AccountType as ProtocolAccountType,
    AssetCallbackFlag,
};
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::merkle::SparseMerklePath;
use miden_protocol::note::{NoteAttachment, NoteAttachmentScheme, NoteType as ProtocolNoteType};
use miden_protocol::testing::account_id::{ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET, ACCOUNT_ID_SENDER};
use miden_protocol::transaction::RawOutputNote;
use miden_protocol::utils::serde::Serializable;
use miden_standards::note::P2idNote;
use miden_standards::testing::note::NoteBuilder;
use miden_testing::{Auth, MockChainBuilder, MockTransactionInput};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

use crate::tests::{create_test_client_builder, seed_mock_transaction_encryption_key};

#[tokio::test]
async fn transport_basic() {
    // Setup entities
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));
    let (mut observer, _observer_account) = create_test_user_transport(mock_node.clone()).await;

    // Create note
    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();

    // Sync-state / fetch notes No notes before sending
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 0);

    // Send note
    sender
        .send_private_note_with_proof(note, &recipient_address, genesis_inclusion_proof())
        .await
        .unwrap();

    // Sync-state / fetch notes 1 note stored
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1);

    // Sync again, should be only 1 note stored
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1);

    // Third user shouldn't receive any note
    observer.sync_state().await.unwrap();
    let notes = observer.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 0);
}

/// Recovers attachments for notes received over NTL. The single-word attachment rides the
/// `SyncNotes` response, so it is recovered with no `GetNotesById` request.
#[tokio::test]
async fn transport_recovers_attachments() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let sender = mock_chain_builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let target = mock_chain_builder.add_existing_wallet(Auth::IncrNonce).unwrap();

    let ntx_target = NetworkAccountTarget::new(target.id(), NoteExecutionHint::Always).unwrap();
    let private_note = NoteBuilder::new(sender.id(), ChaCha20Rng::seed_from_u64(1234))
        .note_type(ProtocolNoteType::Private)
        .tag(NoteTag::new(0).into())
        .attachment(ntx_target)
        .build()
        .unwrap();
    let attachments = private_note.attachments().clone();

    let spawn_note =
        mock_chain_builder.add_spawn_note(std::slice::from_ref(&private_note)).unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();
    let tx = Box::pin(
        mock_chain
            .build_transaction(MockTransactionInput::AccountId(sender.id()))
            .unauthenticated_input_note(spawn_note)
            .expected_output_notes(vec![RawOutputNote::Full(private_note.clone())])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();
    mock_chain.prove_next_block().unwrap();

    // Deliberately not registered on the mock: a single-word attachment reaches the client through
    // the sync record, so a node that withholds it changes nothing.
    let rpc_api = Arc::new(MockRpcApi::new(mock_chain));

    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();
    let mut client = ClientBuilder::new()
        .rpc(rpc_api.clone())
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .note_transport(Arc::new(MockNoteTransportApi::new(mock_node.clone())))
        .tx_discard_delta(None)
        .build()
        .await
        .unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    client.sync_state().await.unwrap();

    client.add_note_tag(private_note.metadata().tag()).await.unwrap();
    mock_node
        .write()
        .add_note(*private_note.header(), NoteDetails::from(private_note.clone()).to_bytes());

    client.fetch_private_notes().await.unwrap();

    let notes = client.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(
        notes[0].attachments(),
        &attachments,
        "note transport recipient should recover attachments from the sync record",
    );
    assert_eq!(
        rpc_api.get_notes_by_id_call_count(),
        0,
        "attachments carried by the sync record need no GetNotesById request",
    );
}

/// A multi-word attachment arrives as a commitment only, so its content must still be fetched. A
/// note whose content the node cannot serve must not fail syncing or NTL fetching.
///
/// The note is skipped per-note, and an NTL-delivered record stays expected rather than being
/// committed without its content, so a later re-import can retry the fetch.
#[tokio::test]
async fn unavailable_attachments_do_not_fail_sync() {
    // The helper tracks the note's tag and syncs to the tip, so it already exercises the sync path:
    // the note advertises attachment content the node cannot serve, and the sync succeeds by
    // skipping the note.
    let (mut client, private_note, mock_transport_node) =
        committed_private_note_recipient(0, true).await;
    assert!(client.get_input_notes(NoteFilter::All).await.unwrap().is_empty());

    // Receiving the same note over the NTL imports it, but it stays expected rather than being
    // committed without its attachment content.
    mock_transport_node
        .write()
        .add_note(*private_note.header(), NoteDetails::from(private_note.clone()).to_bytes());
    client.fetch_private_notes().await.unwrap();

    let notes = client.get_input_notes(NoteFilter::Expected).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert!(notes[0].attachments().is_empty());
}

/// Verifies that cursor-based pagination works: a second sync only receives newly sent notes.
#[tokio::test]
async fn transport_cursor_pagination() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note_a: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();

    let note_b: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();

    // Send note A, sync → recipient receives 1 note
    sender
        .send_private_note_with_proof(note_a.clone(), &recipient_address, genesis_inclusion_proof())
        .await
        .unwrap();
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1, "should have 1 note after first sync");
    // The note is delivered via the transport layer and isn't committed on-chain, so it has no
    // metadata (and thus no `NoteId`); it's identified by its details commitment.
    assert_eq!(notes[0].details_commitment(), note_a.details_commitment());

    // Send note B, sync → recipient receives note B (cursor advanced past A)
    sender
        .send_private_note_with_proof(note_b.clone(), &recipient_address, genesis_inclusion_proof())
        .await
        .unwrap();
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 2, "should have 2 notes total after second sync");
}

/// Fetches more tags than one transport request accepts by splitting them into multiple requests.
#[tokio::test]
async fn transport_fetch_chunks_tracked_tags() {
    const MAX_TAGS: usize = MockClient::<FilesystemKeyStore>::MAX_NOTE_TAGS_PER_TRANSPORT_REQUEST;

    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let transport = MockNoteTransportApi::with_max_tags_per_fetch(mock_node.clone(), MAX_TAGS);
    let (mut recipient, recipient_account) =
        create_test_user_with_transport(Arc::new(transport.clone())).await;

    let added_tags: Vec<NoteTag> = (0..MAX_TAGS)
        .map(|index| NoteTag::new(10_000 + u32::try_from(index).unwrap()))
        .collect();
    for tag in &added_tags {
        recipient.add_note_tag(*tag).await.unwrap();
    }

    let delivery_tag = *added_tags.last().unwrap();
    let note = private_note_with_tag(recipient_account.id(), delivery_tag, 1);
    mock_node
        .write()
        .add_note(*note.header(), NoteDetails::from(note.clone()).to_bytes());

    recipient.fetch_private_notes().await.unwrap();

    assert_eq!(transport.fetch_tag_counts(), vec![MAX_TAGS, 1]);
    let cursors = stored_note_transport_cursors(&mut recipient).await;
    assert_eq!(cursors.len(), MAX_TAGS + 1);
    let delivery_cursor = cursors.get(&delivery_tag).unwrap();
    assert_ne!(*delivery_cursor, NoteTransportCursor::init());
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert!(
        notes
            .iter()
            .any(|record| record.details_commitment() == note.details_commitment())
    );
}

/// Adding a tag starts that tag from the initial cursor without changing existing tag cursors.
#[tokio::test]
async fn transport_adding_tag_preserves_existing_cursors() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let transport = MockNoteTransportApi::new(mock_node.clone());
    let (mut recipient, recipient_account) =
        create_test_user_with_transport(Arc::new(transport.clone())).await;

    let existing_tag = NoteTag::new(20_001);
    let added_tag = NoteTag::new(20_002);
    recipient.add_note_tag(existing_tag).await.unwrap();

    let added_tag_note = private_note_with_tag(recipient_account.id(), added_tag, 1);
    let existing_tag_note = private_note_with_tag(recipient_account.id(), existing_tag, 2);
    mock_node
        .write()
        .add_note(*added_tag_note.header(), NoteDetails::from(added_tag_note.clone()).to_bytes());
    mock_node.write().add_note(
        *existing_tag_note.header(),
        NoteDetails::from(existing_tag_note.clone()).to_bytes(),
    );

    recipient.fetch_private_notes().await.unwrap();
    let cursors_before = stored_note_transport_cursors(&mut recipient).await;
    assert_eq!(transport.fetch_tag_counts(), vec![2]);
    assert!(
        recipient.get_input_notes(NoteFilter::All).await.unwrap().iter().any(|record| {
            record.details_commitment() == existing_tag_note.details_commitment()
        })
    );

    recipient.add_note_tag(added_tag).await.unwrap();
    recipient.fetch_private_notes().await.unwrap();

    let cursors_after = stored_note_transport_cursors(&mut recipient).await;
    assert_eq!(
        cursors_after.get(&existing_tag),
        cursors_before.get(&existing_tag),
        "adding a tag must not change an existing tag cursor"
    );
    assert!(
        cursors_after.get(&added_tag).unwrap() < cursors_after.get(&existing_tag).unwrap(),
        "a new tag must retain the cursor returned for its own history"
    );
    assert_eq!(transport.fetch_tag_counts(), vec![2, 1, 2]);
    assert!(
        recipient
            .get_input_notes(NoteFilter::All)
            .await
            .unwrap()
            .iter()
            .any(|record| record.details_commitment() == added_tag_note.details_commitment())
    );

    recipient.remove_note_tag(added_tag).await.unwrap();
    recipient.fetch_private_notes().await.unwrap();

    let cursors_after_removal = stored_note_transport_cursors(&mut recipient).await;
    assert!(!cursors_after_removal.contains_key(&added_tag));
    assert_eq!(
        cursors_after_removal.get(&existing_tag),
        cursors_after.get(&existing_tag),
        "removing a tag must not change an existing tag cursor"
    );
}

/// Fetches account and user tags while leaving note and subscription tags to the node sync.
#[tokio::test]
async fn transport_fetches_only_ntl_enabled_tag_sources() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let user_tag = NoteTag::new(10_001);
    let note_tag = NoteTag::new(10_002);
    let subscription_tag = NoteTag::new(10_003);
    recipient.add_note_tag(user_tag).await.unwrap();

    let user_note = private_note_with_tag(recipient_account.id(), user_tag, 1);
    let note_source_note = private_note_with_tag(recipient_account.id(), note_tag, 2);
    recipient
        .test_store()
        .add_note_tag(NoteTagRecord::with_note_source(
            note_tag,
            note_source_note.details_commitment(),
        ))
        .await
        .unwrap();
    recipient
        .test_store()
        .add_note_tag(NoteTagRecord {
            tag: subscription_tag,
            source: NoteTagSource::Subscription(Word::default()),
        })
        .await
        .unwrap();
    mock_node
        .write()
        .add_note(*user_note.header(), NoteDetails::from(user_note.clone()).to_bytes());
    mock_node.write().add_note(
        *note_source_note.header(),
        NoteDetails::from(note_source_note.clone()).to_bytes(),
    );

    let records = recipient.get_note_tags().await.unwrap();
    assert!(records.iter().any(|record| matches!(record.source, NoteTagSource::Account(_))));
    assert!(records.iter().any(|record| record.source == NoteTagSource::User));
    assert!(records.iter().any(|record| matches!(record.source, NoteTagSource::Note(_))));
    assert!(
        records
            .iter()
            .any(|record| matches!(record.source, NoteTagSource::Subscription(_)))
    );
    let expected_tags: BTreeSet<NoteTag> = records
        .iter()
        .filter(|record| record.source.is_ntl_enabled())
        .map(|record| record.tag)
        .collect();

    recipient.fetch_private_notes().await.unwrap();

    let fetched_tags: BTreeSet<NoteTag> =
        stored_note_transport_cursors(&mut recipient).await.into_keys().collect();
    assert_eq!(fetched_tags, expected_tags);
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert!(
        notes
            .iter()
            .any(|record| record.details_commitment() == user_note.details_commitment())
    );
    assert!(
        !notes
            .iter()
            .any(|record| record.details_commitment() == note_source_note.details_commitment())
    );
}

/// Advances a tag cursor one server page per sync until the initial backlog is consumed.
#[tokio::test]
async fn transport_tag_cursor_paginates_initial_backlog() {
    const BATCH_CAP: usize = 3;
    const TOTAL_NOTES: usize = 10;

    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::with_max_batch(BATCH_CAP)));
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let tag = NoteTag::new(2002);
    recipient.add_note_tag(tag).await.unwrap();

    // Seed the transport before the first fetch. Building each note before adding it gives each
    // note a distinct mock cursor.
    for i in 0..TOTAL_NOTES {
        let note = private_note_with_tag(recipient_account.id(), tag, 100 + i as u64);
        mock_node.write().add_note(*note.header(), NoteDetails::from(note).to_bytes());
    }

    for page in 1..=TOTAL_NOTES.div_ceil(BATCH_CAP) {
        recipient.sync_state().await.unwrap();
        let expected = (page * BATCH_CAP).min(TOTAL_NOTES);
        assert_eq!(recipient.get_input_notes(NoteFilter::All).await.unwrap().len(), expected);
    }
}

/// Verifies that an observer whose tracked tags don't match the note's tag receives nothing.
#[tokio::test]
async fn transport_fetch_no_matching_tags() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));
    let (mut observer, _observer_account) = create_test_user_transport(mock_node.clone()).await;

    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();

    sender
        .send_private_note_with_proof(note, &recipient_address, genesis_inclusion_proof())
        .await
        .unwrap();

    // Observer syncs — tags don't match, should get nothing
    observer.sync_state().await.unwrap();
    let notes = observer.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 0, "observer with non-matching tags should receive 0 notes");

    // Recipient syncs — tags match, should get the note
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1, "recipient with matching tags should receive 1 note");
}

/// Tests that a private note committed on-chain at the same block the client has synced to is still
/// found when imported via the NTL path. This reproduces the race condition where fast sync (e.g.
/// every 3s) causes `sync_height` to advance past the note's commitment block before the NTL
/// delivers the note details.
#[tokio::test]
async fn fetch_private_notes_finds_note_committed_at_sync_height() {
    // 1. Build a mock chain with a private note committed at block 1.
    let mut mock_chain_builder = MockChainBuilder::new();
    let mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();

    let private_note = NoteBuilder::new(mock_account.id(), ChaCha20Rng::seed_from_u64(1234))
        .note_type(ProtocolNoteType::Private)
        .tag(NoteTag::new(0).into())
        .build()
        .unwrap();

    let spawn_note =
        mock_chain_builder.add_spawn_note(std::slice::from_ref(&private_note)).unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    // Block 1: commit the private note.
    let tx = Box::pin(
        mock_chain
            .build_transaction(MockTransactionInput::AccountId(mock_account.id()))
            .unauthenticated_input_note(spawn_note)
            .expected_output_notes(vec![RawOutputNote::Full(private_note.clone())])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();
    mock_chain.prove_next_block().unwrap();

    // Advance the chain several blocks past the note's commitment block.
    for _ in 0..5 {
        mock_chain.prove_next_block().unwrap();
    }

    // 2. Create client with empty NTL (note not yet delivered).
    let mock_transport_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));

    let rpc_api = MockRpcApi::new(mock_chain);
    let arc_rpc_api = Arc::new(rpc_api);
    let transport_client = MockNoteTransportApi::new(mock_transport_node.clone());

    let keystore_path = temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path.clone()).unwrap();

    let builder: ClientBuilder<FilesystemKeyStore> = ClientBuilder::new()
        .rpc(arc_rpc_api)
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = TestClient::from(builder.build().await.unwrap());
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;

    // 3. Register tag 0 so chain sync sees the note's block.
    client.add_note_tag(NoteTag::new(0)).await.unwrap();

    // 4. Sync to chain tip. The NTL is empty so no transport notes are imported.
    client.sync_state().await.unwrap();
    let sync_height = client.get_sync_height().await.unwrap();
    assert!(sync_height.as_u32() > 1, "client should have synced past block 1");

    // 5. Now the NTL delivers the note (simulates late delivery after the first sync).
    let details = NoteDetails::from(private_note.clone());
    let details_bytes = details.to_bytes();
    mock_transport_node.write().add_note(*private_note.header(), details_bytes);

    // 6. Second sync_state: the transport page is imported, then chain sync runs. The
    // chain scan starts from a lookback window rather than from the sync height, so it still sees
    // the note at block 1.
    let summary = client.sync_state().await.unwrap();
    assert!(
        summary.new_private_notes.contains(&private_note.id()),
        "summary should report the NTL-imported note in new_private_notes"
    );

    // 7. The note should be Committed after the second sync.
    let committed_notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(
        committed_notes.iter().any(|n| n.id() == Some(private_note.id())),
        "note committed before sync_height should be found via lookback during NTL import"
    );
}

/// A private note delivered over the NTL must be committed by the same `sync_state` call that
/// advances past its commitment block.
///
/// The commitment is learned from two independent sources that only combine through the store: the
/// NTL supplies the note's details, which the transport half writes as an `Expected` record, and
/// the node reports the commitment for the note's tag, which the chain half screens with
/// `NoteScreener::on_note_received`. That screening is a store lookup — a private note carries no
/// details from the node, so a record it cannot find is discarded — which makes the order
/// load-bearing: the transport half must write before the chain half screens.
///
/// This is the case the lookback in `fetch_private_notes_finds_note_committed_at_sync_height` does
/// not cover. Here the note commits *above* the client's sync height, so the transport half's own
/// commitment check (capped at the stored sync height) cannot see it and the chain half is the only
/// thing that can. The chain sync's note query is a forward-moving window, so a commitment
/// discarded here is never revisited: the record would stay `Expected` forever.
#[tokio::test]
async fn ntl_note_committed_within_the_sync_window_is_committed_by_that_sync() {
    // 1. Commit a private note at block 1, then advance the chain past it.
    let mut mock_chain_builder = MockChainBuilder::new();
    let mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();

    let private_note = NoteBuilder::new(mock_account.id(), ChaCha20Rng::seed_from_u64(9876))
        .note_type(ProtocolNoteType::Private)
        .tag(NoteTag::new(0).into())
        .build()
        .unwrap();

    let spawn_note =
        mock_chain_builder.add_spawn_note(std::slice::from_ref(&private_note)).unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    let tx = Box::pin(
        mock_chain
            .build_transaction(MockTransactionInput::AccountId(mock_account.id()))
            .unauthenticated_input_note(spawn_note)
            .expected_output_notes(vec![RawOutputNote::Full(private_note.clone())])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();
    mock_chain.prove_next_block().unwrap();

    for _ in 0..5 {
        mock_chain.prove_next_block().unwrap();
    }

    // 2. Build a client that has never synced, so its sync height sits below the note's block.
    let mock_transport_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));

    let rpc_api = Arc::new(MockRpcApi::new(mock_chain));
    let transport_client = MockNoteTransportApi::new(mock_transport_node.clone());
    let rng = ChaCha20Rng::seed_from_u64(1234);

    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();

    let builder: ClientBuilder<FilesystemKeyStore> = ClientBuilder::new()
        .rpc(rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = builder.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;

    client.add_note_tag(NoteTag::new(0)).await.unwrap();

    let sync_height_before = client.get_sync_height().await.unwrap();
    assert_eq!(
        sync_height_before,
        BlockNumber::GENESIS,
        "the note must commit above the sync height for this test to exercise the chain half"
    );

    // 3. Deliver the note over the NTL before that first sync.
    let details_bytes = NoteDetails::from(private_note.clone()).to_bytes();
    mock_transport_node.write().add_note(*private_note.header(), details_bytes);

    // 4. One sync: the transport half receives the details, the chain half reports the commitment
    //    at block 1, and the window (genesis, tip] is consumed.
    client.sync_state().await.unwrap();

    assert!(
        client.get_sync_height().await.unwrap() > BlockNumber::from(1),
        "the sync must have advanced past the note's commitment block"
    );

    let committed_notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(
        committed_notes.iter().any(|note| note.id() == Some(private_note.id())),
        "a delivered note committed inside the synced window must be committed by that sync; \
         leaving it expected strands it, because the chain sync never revisits that block range"
    );
}

/// A note delivered over the NTL whose nullifier is already on chain must be stored as consumed.
///
/// Probe for 0xMiden/rust-sdk#2422.
#[tokio::test]
async fn ntl_note_already_spent_below_the_checkpoint_is_not_left_committed() {
    let sender_id: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
    let faucet_id: AccountId = ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET.try_into().unwrap();

    // 1. Commit a private note to the account, then spend it — both far below the eventual tip.
    let mut builder = MockChainBuilder::new();
    let account = builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let asset = Asset::from(FungibleAsset::new(faucet_id, 100u64).unwrap());
    let note = builder
        .add_p2id_note(sender_id, account.id(), &[asset], ProtocolNoteType::Private)
        .unwrap();

    let mut mock_chain = builder.build().unwrap();
    mock_chain.prove_next_block().unwrap(); // block 1: the note is committed

    let consume_tx = Box::pin(
        mock_chain
            .build_transaction(MockTransactionInput::Account(account.clone()))
            .unauthenticated_input_note(note.clone())
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&consume_tx).unwrap();
    mock_chain.prove_next_block().unwrap(); // block 2: the nullifier is on chain

    for _ in 0..5 {
        mock_chain.prove_next_block().unwrap();
    }

    // 2. A freshly restored client, tracking the note's tag, synced to the tip.
    let mock_transport_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let rpc_api = Arc::new(MockRpcApi::new(mock_chain));
    let transport_client = MockNoteTransportApi::new(mock_transport_node.clone());

    let rng = ChaCha20Rng::seed_from_u64(1234);
    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();

    let builder: ClientBuilder<FilesystemKeyStore> = ClientBuilder::new()
        .rpc(rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = builder.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    client.add_note_tag(note.metadata().tag()).await.unwrap();

    client.sync_state().await.unwrap();
    let checkpoint = client.get_sync_height().await.unwrap();
    assert!(
        checkpoint > BlockNumber::from(2),
        "the spend must sit below the checkpoint for this test to exercise the gap"
    );

    // 3. The transport now re-serves the note, as it does for a cursor-0 client.
    let details_bytes = NoteDetails::from(note.clone()).to_bytes();
    mock_transport_node.write().add_note(*note.header(), details_bytes);

    client.sync_state().await.unwrap();

    // The import must have happened, otherwise the assertion below passes for the wrong reason.
    let all_notes = client.get_input_notes(NoteFilter::All).await.unwrap();
    assert!(
        all_notes.iter().any(|n| n.details_commitment() == note.details_commitment()),
        "the delivered note should have been imported"
    );

    let committed = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(
        !committed.iter().any(|n| n.id() == Some(note.id())),
        "a note whose nullifier is already on chain must not be imported as committed: \
         the forward-only nullifier query never revisits the block that spent it"
    );
}

/// A transport import can resolve an expected note below the checkpoint while the chain sync
/// reports its consumption above the checkpoint.
#[tokio::test]
async fn ntl_refresh_of_expected_note_detects_consumption_in_same_sync() {
    let sender_id: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
    let faucet_id: AccountId = ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET.try_into().unwrap();

    let mut builder = MockChainBuilder::new();
    let account = builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let asset = Asset::from(FungibleAsset::new(faucet_id, 100u64).unwrap());
    let note = builder
        .add_p2id_note(sender_id, account.id(), &[asset], ProtocolNoteType::Private)
        .unwrap();

    let mut mock_chain = builder.build().unwrap();
    mock_chain.prove_next_block().unwrap();

    let consume_tx = Box::pin(
        mock_chain
            .build_transaction(MockTransactionInput::Account(account))
            .unauthenticated_input_note(note.clone())
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    let mock_transport_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let rpc_api = Arc::new(MockRpcApi::new(mock_chain));
    let transport_client = MockNoteTransportApi::new(mock_transport_node.clone());

    let rng = ChaCha20Rng::seed_from_u64(1234);
    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();

    let builder: ClientBuilder<FilesystemKeyStore> = ClientBuilder::new()
        .rpc(rpc_api.clone())
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = builder.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    client.add_note_tag(note.metadata().tag()).await.unwrap();

    client.sync_state().await.unwrap();
    let checkpoint = client.get_sync_height().await.unwrap();
    assert_eq!(checkpoint, BlockNumber::from(1));
    // A later search floor keeps the initial import expected. The transport supplies an earlier
    // floor that resolves its commitment.
    client
        .import_notes(&[NoteFile::ExpectedNote {
            details: NoteDetails::from(note.clone()),
            sync_hint: NoteSyncHint::new(checkpoint + 1, note.metadata().tag()),
        }])
        .await
        .unwrap();
    let expected = client.get_input_notes(NoteFilter::Expected).await.unwrap();
    assert_eq!(expected.len(), 1);
    assert_eq!(expected[0].details_commitment(), note.details_commitment());
    assert!(expected[0].metadata().is_none());

    // The spend is above the checkpoint. The chain sync must check the nullifier supplied by the
    // transport import.
    rpc_api
        .mock_chain
        .write()
        .add_pending_executed_transaction(&consume_tx)
        .unwrap();
    rpc_api.prove_block();

    let details_bytes = NoteDetails::from(note.clone()).to_bytes();
    mock_transport_node.write().add_note_after(
        *note.header(),
        details_bytes,
        Some(BlockNumber::GENESIS),
    );

    client.sync_state().await.unwrap();

    let record = client.get_input_note(note.id()).await.unwrap().unwrap();
    assert!(record.is_consumed(), "the refreshed note must be consumed in the same sync");
    assert_eq!(client.get_sync_height().await.unwrap(), BlockNumber::from(2));

    client.sync_state().await.unwrap();
    let record = client.get_input_note(note.id()).await.unwrap().unwrap();
    assert!(record.is_consumed());
}

/// A private note must reach the recipient even when the sender's first relay attempt fails,
/// provided the transport later recovers.
///
/// `send_private_note_with_proof` persists the payload in a durable outbox, so a relay that fails
/// is retried instead of dropped. Without persistence the recipient would never learn about the
/// note.
///
/// The test does not constrain where the retry happens (inline, on `sync_state`, or through an
/// explicit `flush_relay_outbox`): it polls by alternating sender and recipient `sync_state` calls
/// until the note arrives or the budget is exhausted.
#[tokio::test]
async fn private_note_relay_recovers_after_transient_ntl_failure() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));

    // Fail the next relay attempt, then recover — a single transient transport failure.
    let faulty = Arc::new(FaultyNoteTransportApi::new(mock_node.clone(), 1));
    let (mut sender, sender_account) =
        create_test_user_with_transport(faulty.clone() as Arc<dyn NoteTransportClient>).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    // Transport-delivered notes carry no metadata (hence no `NoteId`); match by details commitment.
    let note_commitment = note.details_commitment();

    // First relay attempt — the faulty NTL rejects it. We don't assert on the return value: the
    // relay may fail here and be retried later.
    let _ = sender
        .send_private_note_with_proof(note, &recipient_address, genesis_inclusion_proof())
        .await;

    // Drive both clients forward; the retry must deliver the note within a few rounds.
    let mut delivered = false;
    for _ in 0..5 {
        let _ = sender.sync_state().await;
        recipient.sync_state().await.unwrap();
        let received = recipient.get_input_notes(NoteFilter::All).await.unwrap();
        if received.iter().any(|n| n.details_commitment() == note_commitment) {
            delivered = true;
            break;
        }
    }

    assert!(
        delivered,
        "a single transient NTL failure permanently lost a private note — sender debited, \
         recipient never learns of it. send_attempts={}",
        faulty.send_attempts()
    );

    // The relay must actually be retried — a single attempt that succeeded by chance is not
    // durability.
    assert!(
        faulty.send_attempts() >= 2,
        "the relay must be retried; observed only {} relay attempt(s)",
        faulty.send_attempts()
    );
}

/// The durable outbox entry survives a failed `send_private_note_with_proof` and is re-sent by an
/// explicit `flush_relay_outbox`, without a full sync. A second flush is a no-op once the entry has
/// drained.
#[tokio::test]
async fn flush_relay_outbox_retries_failed_relay_without_full_sync() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));

    let faulty = Arc::new(FaultyNoteTransportApi::new(mock_node.clone(), 1));
    let (mut sender, sender_account) =
        create_test_user_with_transport(faulty.clone() as Arc<dyn NoteTransportClient>).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    // Transport-delivered notes carry no metadata (hence no `NoteId`); match by details commitment.
    let note_commitment = note.details_commitment();

    // First relay fails; the payload must survive in the outbox.
    let first_attempt = sender
        .send_private_note_with_proof(note, &recipient_address, genesis_inclusion_proof())
        .await;
    assert!(
        first_attempt.is_err(),
        "expected NTL failure on first attempt, got {first_attempt:?}"
    );

    // Recipient sees nothing yet — the NTL never received the note.
    recipient.sync_state().await.unwrap();
    assert!(
        recipient.get_input_notes(NoteFilter::All).await.unwrap().is_empty(),
        "recipient should not yet see the note (NTL was empty after the failed relay)",
    );

    // Explicit flush re-sends (the faulty API has used up its single rejection).
    sender.flush_relay_outbox().await.expect("flush should re-send the queued note");
    assert!(faulty.send_attempts() >= 2, "flush must re-attempt the relay");

    recipient.sync_state().await.unwrap();
    assert!(
        recipient
            .get_input_notes(NoteFilter::All)
            .await
            .unwrap()
            .iter()
            .any(|n| n.details_commitment() == note_commitment),
        "recipient should receive the note after the flush re-send",
    );

    // A second flush is a no-op: the entry was removed when the retry succeeded.
    let attempts_after_first_flush = faulty.send_attempts();
    sender.flush_relay_outbox().await.expect("second flush should succeed (no-op)");
    assert_eq!(
        faulty.send_attempts(),
        attempts_after_first_flush,
        "outbox should be empty after a successful flush; second flush must not re-send",
    );
}

/// A note is routed by the tag in its own metadata, not by who can consume it, and an
/// account-target tag only covers the 14 most significant bits of the account id prefix. The
/// transport fetch screens what a tag match delivers, so a client that is offered a note paying
/// someone else discards it instead of storing it.
#[tokio::test]
async fn note_delivered_by_tag_match_is_only_kept_when_a_tracked_account_can_consume_it() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut client, account) = create_test_user_transport(mock_node.clone()).await;

    // Any account that the client does not track serves as the note's target.
    let unrelated_account: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();

    // A P2ID note only the unrelated account can consume, but carrying the client's account tag.
    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(unrelated_account)
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    // By default the P2ID note is tagged for the target account, so here we manually override the
    // note with the client's account tag.
    let (assets, _, recipient, attachments) = note.into_parts();
    let account_tag = NoteTag::with_account_target(account.id());
    let metadata =
        PartialNoteMetadata::new(sender_account.id(), NoteType::Private).with_tag(account_tag);
    let note = Note::with_attachments(assets, metadata, recipient, attachments);

    // The address is not what routes the note: the relay keys off the tag in its header.
    let address = Address::new(account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));
    sender
        .send_private_note_with_proof(note.clone(), &address, genesis_inclusion_proof())
        .await
        .unwrap();

    // Fetch the notes matching the `account_tag` and check the P2ID note was actually committed
    let (notes_info, _) = mock_node.read().get_notes(&[account_tag], NoteTransportCursor::init());
    let fetched_note_info = notes_info.first().unwrap();
    assert_eq!(fetched_note_info.header, *note.header());

    // During the sync, the client retrieves the note from the NTL (since the tag matches its
    // account), but the note is discarded because its account cannot consume it.
    client.sync_state().await.unwrap();
    let notes = client.get_input_notes(NoteFilter::All).await.unwrap();
    assert!(notes.is_empty(), "a note no tracked account can consume must not be stored");

    // Now send a note consumable by the account and check the client tracks it
    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();

    let recipient_address = Address::new(account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));
    sender
        .send_private_note_with_proof(note, &recipient_address, genesis_inclusion_proof())
        .await
        .unwrap();

    // The client now will track the note during the sync because this time the note is consumable
    // by the tracked account
    client.sync_state().await.unwrap();
    let notes = client.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1, "a note the tracked account can consume must be stored");
}

/// A relay that keeps failing must not block `sync_state`. The outbox flush runs at the start of
/// the transport step; if its error propagated, a single undeliverable note would wedge every
/// subsequent sync. The entry must stay in the outbox for later retry while the sync itself
/// succeeds.
#[tokio::test]
async fn persistent_relay_failure_does_not_block_sync_state() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));

    // Fail effectively forever, modelling a note the NTL never accepts.
    let faulty = Arc::new(FaultyNoteTransportApi::new(mock_node.clone(), usize::MAX));
    let (mut sender, sender_account) =
        create_test_user_with_transport(faulty.clone() as Arc<dyn NoteTransportClient>).await;
    let (_recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();

    // The relay fails and the payload is persisted to the outbox.
    let _ = sender
        .send_private_note_with_proof(note, &recipient_address, genesis_inclusion_proof())
        .await;

    // sync_state flushes the outbox (which fails) but must still complete: the relay failure is
    // logged, not propagated.
    sender
        .sync_state()
        .await
        .expect("sync_state must not fail when an outbox entry can't be relayed");

    // The undeliverable entry is retained for a future attempt, not dropped.
    let direct = sender.flush_relay_outbox().await;
    assert!(
        direct.is_err(),
        "directly flushing an undeliverable entry should surface the error"
    );
}

/// A private note committed more than the fallback lookback window before the recipient's sync
/// height is still found when the sender relays an `after_block_num` floor: the deterministic floor
/// reaches further back than the heuristic would.
#[tokio::test]
async fn fetch_private_notes_uses_sender_provided_after_block_num() {
    // Commit the note at block 1, then advance far enough that the 20-block fallback window
    // (sync_height - 20) starts well above block 1 and would miss it.
    let (mut client, private_note, mock_transport_node) =
        committed_private_note_recipient(30, false).await;

    let sync_height = client.get_sync_height().await.unwrap();
    assert!(
        sync_height.as_u32() > 21,
        "sync height must be beyond the fallback lookback window for this test to be meaningful"
    );

    // Deliver the note WITH a floor pointing at genesis, as the transport does for a note it stored
    // with a verified proof.
    let details_bytes = NoteDetails::from(private_note.clone()).to_bytes();
    mock_transport_node.write().add_note_after(
        *private_note.header(),
        details_bytes,
        Some(BlockNumber::from(0)),
    );

    client.sync_state().await.unwrap();

    let committed_notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(
        committed_notes.iter().any(|n| n.id() == Some(private_note.id())),
        "note should be found via the sender-provided floor even though it predates the lookback \
         window"
    );
}

/// The same scenario without a sender-provided floor: the fallback lookback window starts above the
/// note's commitment block, so the imported note's commitment is not located.
#[tokio::test]
async fn fetch_private_notes_without_floor_falls_back_to_lookback_window() {
    let (mut client, private_note, mock_transport_node) =
        committed_private_note_recipient(30, false).await;

    // Deliver the note WITHOUT a floor: the recipient must rely on the lookback heuristic.
    let details_bytes = NoteDetails::from(private_note.clone()).to_bytes();
    mock_transport_node.write().add_note(*private_note.header(), details_bytes);

    client.sync_state().await.unwrap();

    // The note is imported from the transport layer ...
    let all_notes = client.get_input_notes(NoteFilter::All).await.unwrap();
    assert!(
        all_notes
            .iter()
            .any(|n| n.details_commitment() == private_note.details_commitment()),
        "note should be imported from the transport layer"
    );
    // Its commitment is not located, since the lookback window starts after block 1.
    let committed_notes = client.get_input_notes(NoteFilter::Committed).await.unwrap();
    assert!(
        !committed_notes.iter().any(|n| n.id() == Some(private_note.id())),
        "without a floor the lookback window misses a note committed before sync_height - 20"
    );
}

/// A delivery of a note being consumed locally is skipped and its tag cursor advances (#2345).
#[tokio::test]
async fn transport_delivery_of_processing_note_does_not_wedge_sync_state() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let mut client = Box::pin(create_test_client_transport(mock_node.clone())).await;
    client.sync_state().await.unwrap();

    let account = client.insert_wallet(AccountType::Private).await.unwrap();
    let faucet = client.insert_faucet(AccountType::Private).await.unwrap();

    let mint_request = TransactionRequestBuilder::new()
        .build_mint_fungible_asset(
            FungibleAsset::new(faucet.id(), 5u64).unwrap(),
            account.id(),
            ProtocolNoteType::Public,
            client.rng(),
        )
        .unwrap();
    Box::pin(client.submit_new_transaction(faucet.id(), mint_request.clone()))
        .await
        .unwrap();

    let minted_note = mint_request.expected_output_own_notes().pop().unwrap();
    let note_record = client.get_input_note(minted_note.id()).await.unwrap().unwrap();
    let consume_request = TransactionRequestBuilder::new()
        .input_notes([(note_record.try_into().unwrap(), None)])
        .build()
        .unwrap();
    Box::pin(client.submit_new_transaction(account.id(), consume_request))
        .await
        .unwrap();
    assert!(
        !client.get_input_notes(NoteFilter::Processing).await.unwrap().is_empty(),
        "the consumed note should be in a processing state"
    );

    let cursors_before = stored_note_transport_cursors(&mut client).await;
    // The same note arrives via transport while the consume is in flight.
    mock_node
        .write()
        .add_note(*minted_note.header(), NoteDetails::from(minted_note.clone()).to_bytes());

    let summary = client.sync_state().await.unwrap();
    assert!(
        summary.new_private_notes.is_empty(),
        "the redundant delivery must not be re-imported"
    );

    let cursors_after = stored_note_transport_cursors(&mut client).await;
    assert_eq!(cursors_after.len(), cursors_before.len());
    let mut advanced = false;
    for (tag, cursor_before) in &cursors_before {
        let persisted_cursor = cursors_after.get(tag).expect("each tag must keep its cursor");
        assert!(persisted_cursor >= cursor_before, "a tag cursor must not move backward");
        advanced |= persisted_cursor > cursor_before;
    }
    assert!(advanced, "a tag cursor must advance past the skipped delivery");
    client.sync_state().await.unwrap();

    let records = client.get_input_notes(NoteFilter::All).await.unwrap();
    let matching = records
        .iter()
        .filter(|record| record.details_commitment() == minted_note.details_commitment())
        .count();
    assert_eq!(matching, 1, "the skipped delivery must not create or overwrite a record");
}

/// A failed fetch propagates and leaves the tag cursor state unchanged for retry.
#[tokio::test]
async fn transport_fetch_failure_leaves_cursor_for_retry() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let faulty = Arc::new(FaultyNoteTransportApi::new(mock_node.clone(), 0));
    let (mut recipient, recipient_account) =
        Box::pin(create_test_user_with_transport(faulty.clone())).await;

    let note: Note = P2idNote::builder()
        .sender(recipient_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(recipient.rng())
        .build()
        .unwrap()
        .into();
    mock_node
        .write()
        .add_note(*note.header(), NoteDetails::from(note.clone()).to_bytes());

    faulty.fail_next_n_fetches(2);
    recipient.sync_state().await.unwrap();
    recipient.sync_state().await.unwrap();
    assert_eq!(faulty.fetch_attempts(), 2);
    assert_eq!(recipient.get_input_notes(NoteFilter::All).await.unwrap().len(), 0);

    let summary = recipient.sync_state().await.unwrap();
    assert_eq!(summary.new_private_notes.len(), 1, "note seeded during the outage must arrive");
}

/// A note relayed with its inclusion proof goes through the transport's with-proof path, and the
/// recipient receives the proof's block as the commitment block.
#[tokio::test]
async fn transport_send_with_proof() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    sender
        .send_private_note_with_proof(note.clone(), &recipient_address, genesis_inclusion_proof())
        .await
        .unwrap();

    assert_eq!(mock_node.read().proven_block(&note.id()), Some(BlockNumber::GENESIS));
    let (delivered, _) = mock_node
        .read()
        .get_notes(&[note.metadata().tag()], NoteTransportCursor::init());
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].block_hint, Some(BlockNumber::GENESIS));

    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].details_commitment(), note.details_commitment());
}

/// A relay with a proof that fails stays in the outbox with its proof, and the flush re-sends it
/// through the with-proof path.
#[tokio::test]
async fn flush_relay_outbox_resends_with_proof() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let faulty = Arc::new(FaultyNoteTransportApi::new(mock_node.clone(), 1));
    let (mut sender, sender_account) =
        create_test_user_with_transport(faulty.clone() as Arc<dyn NoteTransportClient>).await;
    let (_recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    let first_attempt = sender
        .send_private_note_with_proof(note.clone(), &recipient_address, genesis_inclusion_proof())
        .await;
    assert!(first_attempt.is_err(), "expected NTL failure on first attempt");
    assert_eq!(mock_node.read().proven_block(&note.id()), None);

    sender.flush_relay_outbox().await.expect("flush should re-send the queued note");
    assert_eq!(faulty.send_attempts(), 2, "flush must re-attempt the relay once");
    assert_eq!(mock_node.read().proven_block(&note.id()), Some(BlockNumber::GENESIS));
}

/// A delivery whose details don't match the header's commitment fails the transport sync. The
/// cursor stays on the page and the chain sync continues.
#[tokio::test]
async fn transport_delivery_with_mismatched_details_errors() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let note_a: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    let note_b: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();

    // Note B's header paired with note A's details.
    mock_node
        .write()
        .add_note(*note_b.header(), NoteDetails::from(note_a.clone()).to_bytes());

    let error = recipient.sync_note_transport().await.unwrap_err();
    match error {
        ClientError::NoteTransportError(NoteTransportError::NoteDetailsMismatch {
            header,
            details,
        }) => {
            assert_eq!(header, note_b.details_commitment());
            assert_eq!(details, note_a.details_commitment());
        },
        other => panic!("expected a details mismatch, got {other:?}"),
    }

    assert_invalid_delivery_is_not_imported(&mut recipient).await;
}

/// A delivery whose details do not decode fails the transport sync.
#[tokio::test]
async fn transport_delivery_with_undecodable_details_errors() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(recipient_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    mock_node.write().add_note(*note.header(), vec![0xff; 4]);

    let error = recipient.sync_note_transport().await.unwrap_err();
    assert!(
        matches!(error, ClientError::NoteTransportError(NoteTransportError::Deserialization(_))),
        "expected a deserialization error, got {error:?}"
    );

    assert_invalid_delivery_is_not_imported(&mut recipient).await;
}

/// A delivery for a tag that was not requested fails the transport sync.
#[tokio::test]
async fn transport_delivery_for_unrequested_tag_errors() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, _recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let tracked_tag = NoteTag::new(777);
    recipient.add_note_tag(tracked_tag).await.unwrap();
    let foreign_note: Note = P2idNote::builder()
        .sender(sender_account.id())
        .target(sender_account.id())
        .asset(dummy_asset())
        .note_type(NoteType::Private)
        .generate_serial_number(sender.rng())
        .build()
        .unwrap()
        .into();
    let foreign_tag = foreign_note.metadata().tag();
    // A note tagged for the sender, served under the recipient's tracked tag.
    mock_node.write().add_note_with_tag_key(
        tracked_tag,
        *foreign_note.header(),
        NoteDetails::from(foreign_note).to_bytes(),
    );

    let error = recipient.sync_note_transport().await.unwrap_err();
    assert!(
        matches!(
            error,
            ClientError::NoteTransportError(NoteTransportError::UnrequestedTag(tag))
                if tag == foreign_tag
        ),
        "expected an unrequested tag error, got {error:?}"
    );

    assert_invalid_delivery_is_not_imported(&mut recipient).await;
}

// HELPERS
// ================================================================================================

/// An inclusion proof that names the genesis block. The mock transport does not verify proofs, so
/// the path is empty; the recipient scans for the commitment from genesis.
fn genesis_inclusion_proof() -> NoteInclusionProof {
    NoteInclusionProof::new(BlockNumber::GENESIS, 0, SparseMerklePath::default()).unwrap()
}

/// A dummy fungible asset for transport-layer notes. P2ID notes require at least one asset, and
/// these notes are never consumed on-chain, so the issuing faucet only needs to be a valid ID.
fn dummy_asset() -> Asset {
    let faucet_id = AccountId::dummy(
        [7u8; 15],
        AccountIdVersion::Version1,
        ProtocolAccountType::Public,
        AssetCallbackFlag::Disabled,
    );
    FungibleAsset::new(faucet_id, 100).unwrap().into()
}

/// Asserts that an invalid delivery on the transport is not imported, that it keeps the stored tag
/// cursors on their pages, and that it does not stop the chain sync.
async fn assert_invalid_delivery_is_not_imported(client: &mut TestClient) {
    let cursors_before = stored_note_transport_cursors(client).await;

    assert!(
        client.sync_note_transport().await.is_err(),
        "the invalid delivery must fail again"
    );
    assert!(
        client.fetch_private_notes().await.is_err(),
        "the invalid delivery must fail again"
    );
    let summary = client.sync_state().await.expect("the chain sync must continue");

    assert!(summary.new_private_notes.is_empty(), "invalid delivery must not import");
    assert_eq!(client.get_input_notes(NoteFilter::All).await.unwrap().len(), 0);
    let cursors_after = stored_note_transport_cursors(client).await;
    assert_eq!(
        cursors_after, cursors_before,
        "tag cursors must not advance past the invalid delivery"
    );
}

async fn stored_note_transport_cursors(
    client: &mut TestClient,
) -> BTreeMap<NoteTag, NoteTransportCursor> {
    let bytes = client
        .test_store()
        .get_setting(SettingScope::Client, String::from(NOTE_TRANSPORT_CURSORS_KEY))
        .await
        .unwrap();

    bytes
        .map(|bytes| BTreeMap::<NoteTag, NoteTransportCursor>::read_from_bytes(&bytes).unwrap())
        .unwrap_or_default()
}

pub async fn create_test_client_transport(
    mock_node: Arc<RwLock<MockNoteTransportNode>>,
) -> TestClient {
    let (builder, _) = create_test_client_builder().await;
    let transport_client = MockNoteTransportApi::new(mock_node);
    let builder_w_transport = builder.note_transport(Arc::new(transport_client));

    let mut client = TestClient::from(builder_w_transport.build().await.unwrap());
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;

    client
}

pub async fn create_test_user_transport(
    mock_node: Arc<RwLock<MockNoteTransportNode>>,
) -> (TestClient, Account) {
    let mut client = Box::pin(create_test_client_transport(mock_node.clone())).await;
    let account = client.insert_wallet(AccountType::Private).await.unwrap();
    (client, account)
}

pub async fn create_test_client_with_transport(
    transport: Arc<dyn NoteTransportClient>,
) -> TestClient {
    let (builder, _) = create_test_client_builder().await;
    let mut client = TestClient::from(builder.note_transport(transport).build().await.unwrap());
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    client
}

pub async fn create_test_user_with_transport(
    transport: Arc<dyn NoteTransportClient>,
) -> (TestClient, Account) {
    let mut client = Box::pin(create_test_client_with_transport(transport)).await;
    let account = client.insert_wallet(AccountType::Private).await.unwrap();
    (client, account)
}

/// Build a private note carrying `tag`, seeded deterministically by `seed` so distinct seeds yield
/// distinct notes. Lets a test seed the mock transport with notes whose tag and relative ordering
/// it controls, independent of any recipient's auto-registered account tag.
fn private_note_with_tag(account: AccountId, tag: NoteTag, seed: u64) -> Note {
    NoteBuilder::new(account, ChaCha20Rng::seed_from_u64(seed))
        .note_type(ProtocolNoteType::Private)
        .tag(tag.into())
        .build()
        .unwrap()
}

/// An attachment spanning more than one word, which a `SyncNotes` response reports as a commitment
/// only, so its content has to be fetched and a node can withhold it.
fn multi_word_attachment() -> NoteAttachment {
    NoteAttachment::with_words(
        NoteAttachmentScheme::new(100).unwrap(),
        vec![Word::from([1u32, 2, 3, 4]), Word::from([5u32, 6, 7, 8])],
    )
    .unwrap()
}

/// Build a chain with a private note (tag 0) committed at block 1, advance `blocks_past_commitment`
/// blocks beyond it, then create a recipient client synced to the tip with an (initially empty)
/// note transport. Returns the client, the committed note, and the shared mock transport node so a
/// test can deliver the note over the NTL afterwards.
///
/// With `with_unserved_attachment` the note carries a multi-word attachment the mock node never
/// serves. It has to be multi-word, since the node sends a single-word one on the sync record.
async fn committed_private_note_recipient(
    blocks_past_commitment: u32,
    with_unserved_attachment: bool,
) -> (TestClient, Note, Arc<RwLock<MockNoteTransportNode>>) {
    let mut mock_chain_builder = MockChainBuilder::new();
    let mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();

    let mut note_builder = NoteBuilder::new(mock_account.id(), ChaCha20Rng::seed_from_u64(1234))
        .note_type(ProtocolNoteType::Private)
        .tag(NoteTag::new(0).into());
    if with_unserved_attachment {
        note_builder = note_builder.attachment(multi_word_attachment());
    }
    let private_note = note_builder.build().unwrap();

    let spawn_note =
        mock_chain_builder.add_spawn_note(std::slice::from_ref(&private_note)).unwrap();
    let mut mock_chain = mock_chain_builder.build().unwrap();

    // Block 1: commit the private note.
    let tx = Box::pin(
        mock_chain
            .build_transaction(MockTransactionInput::AccountId(mock_account.id()))
            .unauthenticated_input_note(spawn_note)
            .expected_output_notes(vec![RawOutputNote::Full(private_note.clone())])
            .build()
            .unwrap()
            .execute(),
    )
    .await
    .unwrap();
    mock_chain.add_pending_executed_transaction(&tx).unwrap();
    mock_chain.prove_next_block().unwrap();

    // Advance the chain past the note's commitment block.
    for _ in 0..blocks_past_commitment {
        mock_chain.prove_next_block().unwrap();
    }

    let mock_transport_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let rpc_api = MockRpcApi::new(mock_chain);
    let arc_rpc_api = Arc::new(rpc_api);
    let transport_client = MockNoteTransportApi::new(mock_transport_node.clone());

    let keystore_path = temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path.clone()).unwrap();

    let builder: ClientBuilder<FilesystemKeyStore> = ClientBuilder::new()
        .rpc(arc_rpc_api)
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = TestClient::from(builder.build().await.unwrap());
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;

    // Register tag 0 so chain sync sees the note's block, then sync to the tip. The NTL is empty,
    // so no transport notes are imported yet.
    client.add_note_tag(NoteTag::new(0)).await.unwrap();
    client.sync_state().await.unwrap();

    (client, private_note, mock_transport_node)
}

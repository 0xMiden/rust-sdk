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
    NoteTag,
    NoteType,
};
use miden_client::note_transport::NoteTransportClient;
use miden_client::store::NoteFilter;
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::{MockClient, MockRpcApi};
use miden_client::testing::note_transport::{
    FaultyNoteTransportApi,
    MockNoteTransportApi,
    MockNoteTransportNode,
};
use miden_client::utils::RwLock;
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::Felt;
use miden_protocol::account::{
    AccountId,
    AccountIdVersion,
    AccountType as ProtocolAccountType,
    AssetCallbackFlag,
};
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::note::NoteType as ProtocolNoteType;
use miden_protocol::testing::account_id::{ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET, ACCOUNT_ID_SENDER};
use miden_protocol::transaction::RawOutputNote;
use miden_protocol::utils::serde::Serializable;
use miden_standards::note::P2idNote;
use miden_standards::testing::note::NoteBuilder;
use miden_testing::{Auth, MockChainBuilder, MockTransactionInput};
use rand::RngExt;

use crate::tests::{
    create_test_client_builder,
    insert_new_wallet,
    seed_mock_transaction_encryption_key,
};

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

    // Sync-state / fetch notes
    // No notes before sending
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 0);

    // Send note
    sender
        .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
        .await
        .unwrap();

    // Sync-state / fetch notes
    // 1 note stored
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

/// Recovers attachments from the node for notes received over NTL.
#[tokio::test]
async fn transport_recovers_attachments() {
    let mut mock_chain_builder = MockChainBuilder::new();
    let sender = mock_chain_builder.add_existing_mock_account(Auth::IncrNonce).unwrap();
    let target = mock_chain_builder.add_existing_wallet(Auth::IncrNonce).unwrap();

    let ntx_target = NetworkAccountTarget::new(target.id(), NoteExecutionHint::Always).unwrap();
    let private_note = NoteBuilder::new(
        sender.id(),
        RandomCoin::new([1, 2, 3, 4].map(Felt::new_unchecked).into()),
    )
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

    let rpc_api = Arc::new(MockRpcApi::new(mock_chain));
    rpc_api.register_private_note_attachments(private_note.id(), attachments.clone());

    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();
    let rng =
        RandomCoin::new(rand::random::<[u64; 4]>().map(|v| Felt::new_unchecked(v >> 1)).into());
    let mut client = ClientBuilder::new()
        .rpc(rpc_api.clone())
        .rng(Box::new(rng))
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
        "note transport recipient should recover attachments via get_notes_by_id",
    );
}

/// A committed note that advertises attachments the node cannot serve must not fail syncing or
/// NTL fetching: the note is skipped per-note, and an NTL-delivered record stays expected (never
/// committed without its attachment content) so a later re-import can retry the fetch.
#[tokio::test]
async fn unavailable_attachments_do_not_fail_sync() {
    // The helper tracks the note's tag and syncs to the tip, so it already exercises the sync
    // path: the note advertises attachment content the node cannot serve, and the sync succeeds
    // by skipping the note.
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
        .send_private_note_with_block_hint(note_a.clone(), &recipient_address, BlockNumber::from(0))
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
        .send_private_note_with_block_hint(note_b.clone(), &recipient_address, BlockNumber::from(0))
        .await
        .unwrap();
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 2, "should have 2 notes total after second sync");
}

/// A tag added after the global cursor has advanced past its notes still receives its history:
/// `sync_note_transport` backfills the newly tracked tag from the start, scoped to that tag alone.
///
/// This is the core regression test for the late-added-tag gap that motivated removing
/// `fetch_all_private_notes`: the steady-state fetch only sees notes past the shared, forward-only
/// cursor, so a tag started late would otherwise never see its older notes.
#[tokio::test]
async fn backfill_imports_history_for_late_added_tag() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let tag_tracked = NoteTag::new(1001);
    let tag_late = NoteTag::new(1002);
    recipient.add_note_tag(tag_tracked).await.unwrap();

    let note_late = private_note_with_tag(recipient_account.id(), tag_late, 10);
    let note_tracked = private_note_with_tag(recipient_account.id(), tag_tracked, 20);

    // Deliver the late tag's note FIRST so it gets the lower cursor, then the tracked tag's note.
    // Syncing the tracked tag advances the global cursor to (or past) the late note's cursor.
    mock_node
        .write()
        .add_note(*note_late.header(), NoteDetails::from(note_late.clone()).to_bytes());
    mock_node
        .write()
        .add_note(*note_tracked.header(), NoteDetails::from(note_tracked.clone()).to_bytes());

    // Sync: only the tracked tag's note is fetched; the late tag isn't tracked yet.
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1, "only the tracked tag's note should arrive first");
    assert!(
        notes
            .iter()
            .any(|n| n.details_commitment() == note_tracked.details_commitment())
    );

    // Track the late tag.
    recipient.add_note_tag(tag_late).await.unwrap();

    // Sync: the backfill must deliver the late tag's note even though its cursor is below the
    // global cursor. The backfill is scoped to the newly tracked tag (it fetches `&[tag_late]`),
    // so it recovers that tag's own history without re-scanning every tag from the start.
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 2, "the late tag's historical note must be backfilled");
    assert!(notes.iter().any(|n| n.details_commitment() == note_late.details_commitment()));
}

/// Removing a tag drops it from the covered set, so re-adding it backfills again. A note that
/// arrives while the tag is untracked, and that another tag then pushes the global cursor past,
/// can only be recovered by a from-the-start backfill. Re-adding the tag must recover it, which
/// proves the covered set is cleared on removal (otherwise the re-added tag would be treated as
/// already covered and the note would be lost).
#[tokio::test]
async fn backfill_recovers_notes_that_arrived_while_untracked() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let tag_x = NoteTag::new(5005);
    let tag_driver = NoteTag::new(5006);
    recipient.add_note_tag(tag_driver).await.unwrap();
    recipient.add_note_tag(tag_x).await.unwrap();

    // Track and cover tag_x while it has no notes yet (so it leaves no `Note`-source tag behind),
    // then stop tracking it.
    recipient.sync_state().await.unwrap();
    recipient.remove_note_tag(tag_x).await.unwrap();

    // While tag_x is untracked, a note arrives for it, followed by a driver-tag note with a higher
    // cursor. Syncing fetches the driver note and advances the global cursor past note_x, so the
    // steady-state fetch can no longer see note_x.
    let note_x = private_note_with_tag(recipient_account.id(), tag_x, 60);
    let note_driver = private_note_with_tag(recipient_account.id(), tag_driver, 70);
    mock_node
        .write()
        .add_note(*note_x.header(), NoteDetails::from(note_x.clone()).to_bytes());
    mock_node
        .write()
        .add_note(*note_driver.header(), NoteDetails::from(note_driver.clone()).to_bytes());
    recipient.sync_state().await.unwrap();

    // note_x is not imported: tag_x was untracked, and it now sits below the global cursor.
    let before = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert!(
        !before.iter().any(|n| n.details_commitment() == note_x.details_commitment()),
        "note_x must not be imported while tag_x is untracked"
    );

    // Re-add tag_x: the backfill drains it from the start and recovers note_x.
    recipient.add_note_tag(tag_x).await.unwrap();
    recipient.sync_state().await.unwrap();
    let after = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert!(
        after.iter().any(|n| n.details_commitment() == note_x.details_commitment()),
        "re-adding a removed tag must backfill notes that arrived while it was untracked"
    );
}

/// The tag backfill drains a tag's history across multiple server-paginated batches.
///
/// Regression test for the interaction between the transport server's response-size cap and the
/// backfill drain loop: a cap of N per response must not leave the backfill returning only the
/// first N notes. With `BATCH_CAP` < the backlog, one sync still pulls the whole history for the
/// newly tracked tag.
#[tokio::test]
async fn backfill_drains_across_batches() {
    const BATCH_CAP: usize = 3;
    const TOTAL_NOTES: usize = 10;

    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::with_max_batch(BATCH_CAP)));
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    let tag_late = NoteTag::new(2002);

    // Seed TOTAL_NOTES > BATCH_CAP notes for the late tag before it is tracked, so a single-batch
    // fetch cannot drain the backlog. Building each note before adding it spaces the mock's
    // timestamp cursors so they stay distinct.
    for i in 0..TOTAL_NOTES {
        let note = private_note_with_tag(recipient_account.id(), tag_late, 100 + i as u64);
        mock_node.write().add_note(*note.header(), NoteDetails::from(note).to_bytes());
    }

    // First sync: the late tag isn't tracked, so none of its notes are fetched.
    recipient.sync_state().await.unwrap();
    assert_eq!(recipient.get_input_notes(NoteFilter::All).await.unwrap().len(), 0);

    // Track the late tag; one sync must drain all TOTAL_NOTES across BATCH_CAP-sized batches.
    recipient.add_note_tag(tag_late).await.unwrap();
    recipient.sync_state().await.unwrap();

    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(
        notes.len(),
        TOTAL_NOTES,
        "backfill must drain the late tag's full history across batches; got {} of {}",
        notes.len(),
        TOTAL_NOTES
    );
}

/// Test that registering more newly tracked tags than the per-sync backfill cap does not lose any
/// tag's history: the burst is spread across syncs, backfilling at most
/// `MAX_BACKFILL_TAGS_PER_SYNC` tags per call and picking up the remainder on the next sync.
#[tokio::test]
async fn backfill_spreads_tags_exceeding_per_sync_cap_across_syncs() {
    const CAP: usize = MockClient::<FilesystemKeyStore>::MAX_BACKFILL_TAGS_PER_SYNC;
    const LATE_TAGS: usize = CAP + 1;

    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;

    // A driver tag tracked from the start pushes the global cursor forward. The late tags' notes
    // are delivered before the driver note, so they sit below the advanced cursor and can only be
    // recovered by the from-the-start backfill, not the steady-state fetch.
    let driver_tag = NoteTag::new(9_999);
    recipient.add_note_tag(driver_tag).await.unwrap();

    let late_tags: Vec<NoteTag> = (0..LATE_TAGS)
        .map(|i| NoteTag::new(3_000 + u32::try_from(i).unwrap()))
        .collect();
    for (i, tag) in late_tags.iter().enumerate() {
        let note = private_note_with_tag(recipient_account.id(), *tag, 100 + i as u64);
        mock_node.write().add_note(*note.header(), NoteDetails::from(note).to_bytes());
    }

    // Deliver the driver note last so it takes the highest cursor.
    let driver_note = private_note_with_tag(recipient_account.id(), driver_tag, 10_000);
    mock_node
        .write()
        .add_note(*driver_note.header(), NoteDetails::from(driver_note.clone()).to_bytes());

    // First sync: only the driver tag is tracked, so just its note arrives and the global cursor
    // advances past every late tag's note.
    recipient.sync_state().await.unwrap();
    assert_eq!(
        recipient.get_input_notes(NoteFilter::All).await.unwrap().len(),
        1,
        "only the driver tag's note should arrive first"
    );

    // Track all LATE_TAGS at once, exceeding the per-sync backfill cap by one.
    for tag in &late_tags {
        recipient.add_note_tag(*tag).await.unwrap();
    }

    // Second sync: the backfill covers at most CAP late tags, so one late note stays uncovered.
    // Total = driver note + capped backfill.
    recipient.sync_state().await.unwrap();
    assert_eq!(
        recipient.get_input_notes(NoteFilter::All).await.unwrap().len(),
        1 + CAP,
        "one sync must backfill at most MAX_BACKFILL_TAGS_PER_SYNC tags"
    );

    // Third sync: the deferred late tag is backfilled, recovering the whole history.
    recipient.sync_state().await.unwrap();
    assert_eq!(
        recipient.get_input_notes(NoteFilter::All).await.unwrap().len(),
        1 + LATE_TAGS,
        "the deferred tag must be backfilled on the following sync"
    );
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
        .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
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

/// Tests that a private note committed on-chain at the same block the client has synced to
/// is still found when imported via the NTL path. This reproduces the race condition where
/// fast sync (e.g. every 3s) causes `sync_height` to advance past the note's commitment
/// block before the NTL delivers the note details.
#[tokio::test]
async fn fetch_private_notes_finds_note_committed_at_sync_height() {
    // 1. Build a mock chain with a private note committed at block 1.
    let mut mock_chain_builder = MockChainBuilder::new();
    let mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();

    let private_note = NoteBuilder::new(
        mock_account.id(),
        RandomCoin::new([1, 2, 3, 4].map(Felt::new_unchecked).into()),
    )
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

    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();
    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());

    let keystore_path = temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path.clone()).unwrap();

    let builder: ClientBuilder<FilesystemKeyStore> = ClientBuilder::new()
        .rpc(arc_rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = builder.build().await.unwrap();
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

    // 6. Second sync_state: fetch_transport_notes imports the note, then chain sync runs.
    // Without the fix, after_block_num = sync_height, scan misses the note at block 1.
    // With the fix, lookback window catches it.
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
/// The commitment is learned from two independent sources that only combine through the store:
/// the NTL supplies the note's details, which the transport half writes as an `Expected` record,
/// and the node reports the commitment for the note's tag, which the chain half screens with
/// `NoteScreener::on_note_received`. That screening is a store lookup — a private note carries no
/// details from the node, so a record it cannot find is discarded — which makes the order
/// load-bearing: the transport half must write before the chain half screens.
///
/// This is the case the lookback in `fetch_private_notes_finds_note_committed_at_sync_height`
/// does not cover. Here the note commits *above* the client's sync height, so the transport half's
/// own commitment check (capped at the stored sync height) cannot see it and the chain half is the
/// only thing that can. The chain sync's note query is a forward-moving window, so a commitment
/// discarded here is never revisited: the record would stay `Expected` forever.
#[tokio::test]
async fn ntl_note_committed_within_the_sync_window_is_committed_by_that_sync() {
    // 1. Commit a private note at block 1, then advance the chain past it.
    let mut mock_chain_builder = MockChainBuilder::new();
    let mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();

    let private_note = NoteBuilder::new(
        mock_account.id(),
        RandomCoin::new([9, 8, 7, 6].map(Felt::new_unchecked).into()),
    )
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

    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();
    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());

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
    let asset = Asset::Fungible(FungibleAsset::new(faucet_id, 100u64).unwrap());
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

    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();
    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());
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

/// A private note must reach the recipient even when the sender's first relay
/// attempt fails, provided the transport later recovers.
///
/// Without the durable outbox, `send_private_note` relays the payload exactly
/// once; if that call fails the payload is dropped (no retry, no persistence)
/// and the recipient never learns about the note. The outbox makes the relay
/// retriable, so a transient transport failure no longer loses the note.
///
/// The test doesn't constrain the fix's shape (inline retry, retry on
/// `sync_state`, or an explicit `flush_relay_outbox`): it polls by alternating
/// sender/recipient `sync_state` calls until the note arrives or the budget is
/// exhausted.
#[tokio::test]
async fn private_note_relay_recovers_after_transient_ntl_failure() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));

    // Fail the next send_note attempt, then recover — a single transient
    // transport failure.
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
    // Transport-delivered notes carry no metadata (hence no `NoteId`); match by
    // details commitment.
    let note_commitment = note.details_commitment();

    // First relay attempt — the faulty NTL rejects it. We don't assert on the
    // return value: the relay may fail here and be retried later.
    let _ = sender
        .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
        .await;

    // Drive both clients forward; the retry must deliver the note within a few
    // rounds.
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

    // The fix must actually retry the relay — a single attempt that succeeded
    // by chance is not durability.
    assert!(
        faulty.send_attempts() >= 2,
        "fix must retry the relay; observed only {} send_note attempt(s)",
        faulty.send_attempts()
    );
}

/// The durable outbox entry survives a failed `send_private_note` and is
/// re-sent by an explicit `flush_relay_outbox`, without a full sync. A second
/// flush is a no-op once the entry has drained.
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
    // Transport-delivered notes carry no metadata (hence no `NoteId`); match by
    // details commitment.
    let note_commitment = note.details_commitment();

    // First relay fails; the payload must survive in the outbox.
    let first_attempt = sender
        .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
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

/// A relay that keeps failing must not block `sync_state`. The outbox flush
/// runs at the start of the transport step; if its error propagated, a single
/// undeliverable note would wedge every subsequent sync. The entry must stay in
/// the outbox for later retry while the sync itself succeeds.
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
        .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
        .await;

    // sync_state flushes the outbox (which fails) but must still complete: the
    // relay failure is logged, not propagated.
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

/// `send_private_note_with_block_hint` delivers a note end-to-end like `send_private_note`,
/// exercising the floor-carrying relay path.
#[tokio::test]
async fn send_private_note_with_block_hint_delivers_note() {
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
        .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
        .await
        .unwrap();

    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1, "recipient should receive the note relayed with a block floor");
}

/// A private note committed more than the fallback lookback window before the recipient's sync
/// height is still found when the sender relays an `after_block_num` floor: the deterministic
/// floor reaches further back than the heuristic would.
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

    // Deliver the note WITH a floor pointing at genesis, mirroring
    // `send_private_note_with_block_hint`.
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

/// The same scenario without a sender-provided floor: the fallback lookback window starts above
/// the note's commitment block, so the imported note's commitment is not located.
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

// HELPERS
// ================================================================================================

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

pub async fn create_test_client_transport(
    mock_node: Arc<RwLock<MockNoteTransportNode>>,
) -> (MockClient<FilesystemKeyStore>, FilesystemKeyStore) {
    let (builder, _, keystore) = create_test_client_builder().await;
    let transport_client = MockNoteTransportApi::new(mock_node);
    let builder_w_transport = builder.note_transport(Arc::new(transport_client));

    let mut client = builder_w_transport.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;

    (client, keystore)
}

pub async fn create_test_user_transport(
    mock_node: Arc<RwLock<MockNoteTransportNode>>,
) -> (MockClient<FilesystemKeyStore>, Account) {
    let (mut client, keystore) = Box::pin(create_test_client_transport(mock_node.clone())).await;
    let account = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();
    (client, account)
}

pub async fn create_test_client_with_transport(
    transport: Arc<dyn NoteTransportClient>,
) -> (MockClient<FilesystemKeyStore>, FilesystemKeyStore) {
    let (builder, _, keystore) = create_test_client_builder().await;
    let mut client = builder.note_transport(transport).build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;
    (client, keystore)
}

pub async fn create_test_user_with_transport(
    transport: Arc<dyn NoteTransportClient>,
) -> (MockClient<FilesystemKeyStore>, Account) {
    let (mut client, keystore) = Box::pin(create_test_client_with_transport(transport)).await;
    let account = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();
    (client, account)
}

/// Build a private note carrying `tag`, seeded deterministically by `seed` so distinct seeds yield
/// distinct notes. Lets a test seed the mock transport with notes whose tag and relative ordering
/// it controls, independent of any recipient's auto-registered account tag.
fn private_note_with_tag(account: AccountId, tag: NoteTag, seed: u64) -> Note {
    NoteBuilder::new(
        account,
        RandomCoin::new([seed, seed + 1, seed + 2, seed + 3].map(Felt::new_unchecked).into()),
    )
    .note_type(ProtocolNoteType::Private)
    .tag(tag.into())
    .build()
    .unwrap()
}

/// Build a chain with a private note (tag 0) committed at block 1, advance
/// `blocks_past_commitment` blocks beyond it, then create a recipient client synced to the tip
/// with an (initially empty) note transport. Returns the client, the committed note, and the
/// shared mock transport node so a test can deliver the note over the NTL afterwards.
///
/// With `with_unserved_attachment` the note's metadata advertises an attachment whose content is
/// never registered with the mock node, so any content fetch for the note comes back empty.
async fn committed_private_note_recipient(
    blocks_past_commitment: u32,
    with_unserved_attachment: bool,
) -> (MockClient<FilesystemKeyStore>, Note, Arc<RwLock<MockNoteTransportNode>>) {
    let mut mock_chain_builder = MockChainBuilder::new();
    let mock_account = mock_chain_builder
        .add_existing_mock_account(miden_testing::Auth::IncrNonce)
        .unwrap();

    let mut note_builder = NoteBuilder::new(
        mock_account.id(),
        RandomCoin::new([1, 2, 3, 4].map(Felt::new_unchecked).into()),
    )
    .note_type(ProtocolNoteType::Private)
    .tag(NoteTag::new(0).into());
    if with_unserved_attachment {
        let ntx_target =
            NetworkAccountTarget::new(mock_account.id(), NoteExecutionHint::Always).unwrap();
        note_builder = note_builder.attachment(ntx_target);
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

    let mut rng = rand::rng();
    let coin_seed: [u64; 4] = rng.random();
    let rng = RandomCoin::new(coin_seed.map(|v| Felt::new_unchecked(v >> 1)).into());

    let keystore_path = temp_dir();
    let keystore = FilesystemKeyStore::new(keystore_path.clone()).unwrap();

    let builder: ClientBuilder<FilesystemKeyStore> = ClientBuilder::new()
        .rpc(arc_rpc_api)
        .rng(Box::new(rng))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = builder.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();
    seed_mock_transaction_encryption_key(&mut client).await;

    // Register tag 0 so chain sync sees the note's block, then sync to the tip. The NTL is empty,
    // so no transport notes are imported yet.
    client.add_note_tag(NoteTag::new(0)).await.unwrap();
    client.sync_state().await.unwrap();

    (client, private_note, mock_transport_node)
}

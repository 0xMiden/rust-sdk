use std::env::temp_dir;
use std::sync::Arc;

use miden_client::DebugMode;
use miden_client::account::{Account, AccountType};
use miden_client::address::{Address, AddressInterface, RoutingParameters};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::{Note, NoteAttachments, NoteDetails, NoteTag, NoteType};
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
use miden_protocol::block::BlockNumber;
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::note::NoteType as ProtocolNoteType;
use miden_protocol::transaction::RawOutputNote;
use miden_protocol::utils::serde::Serializable;
use miden_standards::note::P2idNote;
use miden_standards::testing::note::NoteBuilder;
use miden_testing::{MockChainBuilder, TxContextInput};
use rand::Rng;

use crate::tests::{create_test_client_builder, insert_new_wallet};

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
    let note = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();

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

/// Verifies that cursor-based pagination works: a second sync only receives newly sent notes.
#[tokio::test]
async fn transport_cursor_pagination() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note_a = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();

    let note_b = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();

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

/// Verifies that `fetch_all_private_notes` (cursor reset) does not duplicate notes in the store.
#[tokio::test]
async fn transport_duplicate_note_handling() {
    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::new()));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    let note = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();

    sender
        .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
        .await
        .unwrap();

    // First fetch
    recipient.sync_state().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1);

    // Reset cursor and re-fetch everything
    recipient.fetch_all_private_notes().await.unwrap();
    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1, "should still have 1 note, not duplicated");
}

/// Verifies that `fetch_all_private_notes` drains notes across multiple
/// server-paginated batches.
///
/// Regression test for the interaction between the transport server's
/// response-size `LIMIT` and the client's previously-single-shot
/// `fetch_all_private_notes`. Before the drain loop, a server cap of N per
/// response meant `fetch_all_private_notes` silently returned only the first
/// N notes and the rest were invisible until the next paginated sync tick.
#[tokio::test]
async fn fetch_all_private_notes_drains_across_batches() {
    const BATCH_CAP: usize = 3;
    const TOTAL_NOTES: usize = 10;

    let mock_node = Arc::new(RwLock::new(MockNoteTransportNode::with_max_batch(BATCH_CAP)));
    let (mut sender, sender_account) = create_test_user_transport(mock_node.clone()).await;
    let (mut recipient, recipient_account) = create_test_user_transport(mock_node.clone()).await;
    let recipient_address = Address::new(recipient_account.id())
        .with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));

    // Send TOTAL_NOTES > BATCH_CAP private notes so a single-batch fetch
    // cannot drain the backlog.
    for _ in 0..TOTAL_NOTES {
        let note = P2idNote::create(
            sender_account.id(),
            recipient_account.id(),
            vec![],
            NoteType::Private,
            NoteAttachments::empty(),
            sender.rng(),
        )
        .unwrap();
        sender
            .send_private_note_with_block_hint(note, &recipient_address, BlockNumber::from(0))
            .await
            .unwrap();
    }

    // With BATCH_CAP=3 and TOTAL_NOTES=10, a single-shot fetch would return
    // only 3. The drain loop should issue successive calls until all 10 are
    // pulled.
    recipient.fetch_all_private_notes().await.unwrap();

    let notes = recipient.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(
        notes.len(),
        TOTAL_NOTES,
        "fetch_all_private_notes must drain across batches; got {} of {}",
        notes.len(),
        TOTAL_NOTES
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

    let note = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();

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
            .build_tx_context(TxContextInput::AccountId(mock_account.id()), &[], &[spawn_note])
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
        .in_debug_mode(DebugMode::Enabled)
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = builder.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();

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

    let note = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();
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

    let note = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();
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

    let note = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();

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

    let note = P2idNote::create(
        sender_account.id(),
        recipient_account.id(),
        vec![],
        NoteType::Private,
        NoteAttachments::empty(),
        sender.rng(),
    )
    .unwrap();

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
        committed_private_note_recipient(30).await;

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
        committed_private_note_recipient(30).await;

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

pub async fn create_test_client_transport(
    mock_node: Arc<RwLock<MockNoteTransportNode>>,
) -> (MockClient<FilesystemKeyStore>, FilesystemKeyStore) {
    let (builder, _, keystore) = create_test_client_builder().await;
    let transport_client = MockNoteTransportApi::new(mock_node);
    let builder_w_transport = builder.note_transport(Arc::new(transport_client));

    let mut client = builder_w_transport.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();

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
    (client, keystore)
}

pub async fn create_test_user_with_transport(
    transport: Arc<dyn NoteTransportClient>,
) -> (MockClient<FilesystemKeyStore>, Account) {
    let (mut client, keystore) = Box::pin(create_test_client_with_transport(transport)).await;
    let account = insert_new_wallet(&mut client, AccountType::Private, &keystore).await.unwrap();
    (client, account)
}

/// Build a chain with a private note (tag 0) committed at block 1, advance
/// `blocks_past_commitment` blocks beyond it, then create a recipient client synced to the tip
/// with an (initially empty) note transport. Returns the client, the committed note, and the
/// shared mock transport node so a test can deliver the note over the NTL afterwards.
async fn committed_private_note_recipient(
    blocks_past_commitment: u32,
) -> (MockClient<FilesystemKeyStore>, Note, Arc<RwLock<MockNoteTransportNode>>) {
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
            .build_tx_context(TxContextInput::AccountId(mock_account.id()), &[], &[spawn_note])
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
        .in_debug_mode(DebugMode::Enabled)
        .tx_discard_delta(None)
        .note_transport(Arc::new(transport_client));

    let mut client = builder.build().await.unwrap();
    client.ensure_genesis_in_place().await.unwrap();

    // Register tag 0 so chain sync sees the note's block, then sync to the tip. The NTL is empty,
    // so no transport notes are imported yet.
    client.add_note_tag(NoteTag::new(0)).await.unwrap();
    client.sync_state().await.unwrap();

    (client, private_note, mock_transport_node)
}

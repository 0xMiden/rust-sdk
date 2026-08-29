use std::sync::Arc;

use miden_client::note::{
    InputNoteReader,
    NoteAssets,
    NoteAttachments,
    NoteMetadata,
    NoteRecipient,
    NoteStorage,
    NoteTag,
    NoteType,
    NoteUpdateTracker,
    PartialNoteMetadata,
};
use miden_client::store::input_note_states::{
    ConsumedExternalNoteState,
    ConsumedUnauthenticatedLocalNoteState,
    ExpectedNoteState,
    NoteSubmissionData,
};
use miden_client::store::{
    InputNoteCursor,
    InputNoteRecord,
    InputNoteState,
    NoteFilter,
    OutputNoteRecord,
    OutputNoteState,
    Store,
};
use miden_client::sync::{
    AccountUpdates,
    PartialBlockchainUpdates,
    StateSyncUpdate,
    TransactionUpdateTracker,
};
use miden_client::utils::{Deserializable, DeserializationError, Serializable};
use miden_client::{Felt, ZERO};
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{NoteAttachment, NoteAttachmentScheme, NoteDetails, NoteScript};
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET,
    ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE,
};
use miden_protocol::transaction::TransactionId;
use miden_standards::note::StandardNote;

use crate::tests::create_test_store;

// HELPERS
// ================================================================================================

/// Helper to build the metadata of a note sent by the given account.
fn create_note_metadata(sender: AccountId, index: u32) -> NoteMetadata {
    let partial_metadata =
        PartialNoteMetadata::new(sender, NoteType::Public).with_tag(NoteTag::from(index));
    NoteMetadata::new(partial_metadata, &NoteAttachments::empty())
}

/// Helper to create a consumed-external input note with an optional consumer account. A note
/// without metadata has no nullifier, so its column is NULL.
fn create_consumed_external_input_note(
    index: u32,
    block_height: u32,
    consumer_account: Option<AccountId>,
    metadata: Option<NoteMetadata>,
) -> InputNoteRecord {
    let serial_number: Word =
        [Felt::new_unchecked(u64::from(index) + 2000), ZERO, ZERO, ZERO].into();
    let assets = NoteAssets::new(vec![]).unwrap();
    let recipient = NoteRecipient::new(
        serial_number,
        StandardNote::SWAP.script(),
        NoteStorage::new(vec![]).unwrap(),
    );
    let details = NoteDetails::new(assets, recipient);

    let state = ConsumedExternalNoteState {
        nullifier_block_height: BlockNumber::from(block_height),
        consumer_account,
        consumed_tx_order: None,
        metadata,
    };

    InputNoteRecord::new(details, NoteAttachments::empty(), Some(0), state.into())
}

/// Helper to create an expected (non-consumed) input note.
fn create_expected_input_note(index: u32) -> InputNoteRecord {
    create_expected_input_note_with_script(index, StandardNote::SWAP.script())
}

/// Helper to create an expected (non-consumed) input note with a specific script.
fn create_expected_input_note_with_script(index: u32, script: NoteScript) -> InputNoteRecord {
    let serial_number: Word =
        [Felt::new_unchecked(u64::from(index) + 3000), ZERO, ZERO, ZERO].into();
    let assets = NoteAssets::new(vec![]).unwrap();
    let recipient = NoteRecipient::new(serial_number, script, NoteStorage::new(vec![]).unwrap());
    let details = NoteDetails::new(assets, recipient);

    let state = ExpectedNoteState {
        metadata: None,
        after_block_num: BlockNumber::from(0u32),
        tag: None,
    };

    InputNoteRecord::new(details, NoteAttachments::empty(), Some(0), state.into())
}

/// Helper to create an expected (non-consumed) input note that carries metadata, so it has a
/// known nullifier.
fn create_expected_input_note_with_metadata(index: u32) -> InputNoteRecord {
    let serial_number: Word =
        [Felt::new_unchecked(u64::from(index) + 9000), ZERO, ZERO, ZERO].into();
    let assets = NoteAssets::new(vec![]).unwrap();
    let recipient = NoteRecipient::new(
        serial_number,
        StandardNote::SWAP.script(),
        NoteStorage::new(vec![]).unwrap(),
    );
    let details = NoteDetails::new(assets, recipient);

    let sender = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let state = ExpectedNoteState {
        metadata: Some(create_note_metadata(sender, index)),
        after_block_num: BlockNumber::from(0u32),
        tag: None,
    };

    InputNoteRecord::new(details, NoteAttachments::empty(), Some(0), state.into())
}

/// Helper to create an expected output note with a specific script.
fn create_expected_output_note_with_script(index: u32, script: NoteScript) -> OutputNoteRecord {
    let serial_number: Word =
        [Felt::new_unchecked(u64::from(index) + 7000), ZERO, ZERO, ZERO].into();
    let recipient = NoteRecipient::new(serial_number, script, NoteStorage::new(vec![]).unwrap());
    let sender = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    OutputNoteRecord::new(
        recipient.digest(),
        NoteAssets::new(vec![]).unwrap(),
        create_note_metadata(sender, index),
        OutputNoteState::ExpectedFull { recipient },
        BlockNumber::from(0u32),
        NoteAttachments::empty(),
    )
}

/// Helper to create a consumed-unauthenticated-local input note with a specific consumer.
fn create_consumed_input_note_with_consumer(
    consumer: AccountId,
    index: u32,
    block_height: u32,
    consumed_tx_order: u32,
) -> InputNoteRecord {
    let serial_number: Word =
        [Felt::new_unchecked(u64::from(index) + 5000), ZERO, ZERO, ZERO].into();
    let assets = NoteAssets::new(vec![]).unwrap();
    let recipient = NoteRecipient::new(
        serial_number,
        StandardNote::SWAP.script(),
        NoteStorage::new(vec![]).unwrap(),
    );
    let details = NoteDetails::new(assets, recipient);

    let state = ConsumedUnauthenticatedLocalNoteState {
        metadata: create_note_metadata(consumer, index),
        nullifier_block_height: BlockNumber::from(block_height),
        submission_data: NoteSubmissionData {
            submitted_at: Some(0),
            consumer_account: consumer,
            consumer_transaction: TransactionId::from_raw(Word::default()),
        },
        consumed_tx_order: Some(consumed_tx_order),
    };

    InputNoteRecord::new(details, NoteAttachments::empty(), Some(0), state.into())
}

/// Returns the key that the per-account consumption order sorts by: consumption block height,
/// transaction order within that block and, as the tie-break, the details commitment.
fn consumption_key(note: &InputNoteRecord) -> (u32, u32, Vec<u8>) {
    (
        note.state().consumed_block_height().expect("note is consumed").as_u32(),
        note.state().consumed_tx_order().expect("note has a consumption order"),
        note.details_commitment().to_bytes(),
    )
}

/// Drains `reader`, returning the consumption key of every note it yields.
async fn walk(reader: &mut InputNoteReader) -> Vec<(u32, u32, Vec<u8>)> {
    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(consumption_key(&note));
    }
    collected
}

// INPUT NOTE READER TESTS
// ================================================================================================

#[tokio::test]
async fn input_note_reader_returns_none_on_empty_store() {
    let store = create_test_store().await;
    let store: Arc<dyn Store> = Arc::new(store);
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let mut reader = InputNoteReader::new(store, consumer);
    let result = reader.next().await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn input_note_reader_iterates_all_consumed_notes() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let notes: Vec<_> = (0..3u32)
        .map(|i| create_consumed_input_note_with_consumer(consumer, i, 1, 0))
        .collect();
    store.upsert_input_notes(&notes).await.unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer);

    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(note);
    }

    assert_eq!(collected.len(), 3);
}

#[tokio::test]
async fn input_note_reader_skips_non_consumed_notes() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    // Insert 2 consumed notes and 1 expected note.
    let consumed1 = create_consumed_input_note_with_consumer(consumer, 0, 1, 0);
    let expected = create_expected_input_note(1);
    let consumed2 = create_consumed_input_note_with_consumer(consumer, 2, 1, 1);

    store.upsert_input_notes(&[consumed1, expected, consumed2]).await.unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer);

    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(note);
    }

    // Only the 2 consumed notes should be returned.
    assert_eq!(collected.len(), 2);
}

#[tokio::test]
async fn input_note_reader_filters_by_consumer() {
    let store = create_test_store().await;
    let consumer_a =
        AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
    let consumer_b = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap();

    // Two notes for consumer_a with tx_order, one for consumer_b with tx_order.
    let note_a1 = create_consumed_input_note_with_consumer(consumer_a, 10, 1, 0);
    let note_b = create_consumed_input_note_with_consumer(consumer_b, 11, 1, 0);
    let note_a2 = create_consumed_input_note_with_consumer(consumer_a, 12, 1, 1);

    store.upsert_input_notes(&[note_a1, note_b, note_a2]).await.unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer_a);

    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(note);
    }

    assert_eq!(collected.len(), 2);
    for note in &collected {
        assert_eq!(note.consumer_account(), Some(consumer_a));
    }
}

#[tokio::test]
async fn input_note_reader_excludes_notes_without_tx_order_when_consumer_is_set() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    // Insert two notes for the same consumer: one with tx_order, one without.
    let note_with_order = create_consumed_input_note_with_consumer(consumer, 30, 1, 0);
    let mut note_without_order = create_consumed_input_note_with_consumer(consumer, 31, 1, 0);
    note_without_order.set_consumed_tx_order(None);

    store
        .upsert_input_notes(&[note_with_order.clone(), note_without_order])
        .await
        .unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer);

    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(note);
    }

    // Only the note with tx_order should be returned.
    assert_eq!(collected.len(), 1);
    assert_eq!(collected[0].id(), note_with_order.id());
}

#[tokio::test]
async fn input_note_reader_filters_by_block_range() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    // Create consumed notes at different block heights.
    let note_b1 = create_consumed_input_note_with_consumer(consumer, 0, 1, 0);
    let note_b3 = create_consumed_input_note_with_consumer(consumer, 1, 3, 0);
    let note_b5 = create_consumed_input_note_with_consumer(consumer, 2, 5, 0);
    let note_b7 = create_consumed_input_note_with_consumer(consumer, 3, 7, 0);

    store
        .upsert_input_notes(&[note_b1, note_b3.clone(), note_b5.clone(), note_b7])
        .await
        .unwrap();

    let store: Arc<dyn Store> = Arc::new(store);

    // Filter to blocks 3..=5
    let mut reader = InputNoteReader::new(store, consumer)
        .in_block_range(BlockNumber::from(3u32), BlockNumber::from(5u32));

    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(note);
    }

    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].id(), note_b3.id());
    assert_eq!(collected[1].id(), note_b5.id());
}

#[tokio::test]
async fn input_note_reader_filters_by_consumer_and_block_range() {
    let store = create_test_store().await;
    let consumer_a =
        AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
    let consumer_b = AccountId::try_from(ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET).unwrap();

    // consumer_a at blocks 1, 3, 5; consumer_b at block 3.
    let alice_at_1 = create_consumed_input_note_with_consumer(consumer_a, 20, 1, 0);
    let alice_at_3 = create_consumed_input_note_with_consumer(consumer_a, 21, 3, 0);
    let bob_at_3 = create_consumed_input_note_with_consumer(consumer_b, 22, 3, 1);
    let alice_at_5 = create_consumed_input_note_with_consumer(consumer_a, 23, 5, 0);

    store
        .upsert_input_notes(&[alice_at_1, alice_at_3.clone(), bob_at_3, alice_at_5.clone()])
        .await
        .unwrap();

    let store: Arc<dyn Store> = Arc::new(store);

    // Filter to consumer_a in blocks 3..=5 — should return alice_at_3 and alice_at_5 only.
    let mut reader = InputNoteReader::new(store, consumer_a)
        .in_block_range(BlockNumber::from(3u32), BlockNumber::from(5u32));

    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(note);
    }

    assert_eq!(collected.len(), 2);
    assert_eq!(collected[0].id(), alice_at_3.id());
    assert_eq!(collected[1].id(), alice_at_5.id());
    for note in &collected {
        assert_eq!(note.consumer_account(), Some(consumer_a));
    }
}

#[tokio::test]
async fn input_note_reader_finds_externally_consumed_notes() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let mut tracked_note = create_consumed_external_input_note(0, 1, Some(consumer), None);
    tracked_note.set_consumed_tx_order(Some(0));

    let mut untracked_note = create_consumed_external_input_note(1, 2, None, None);
    untracked_note.set_consumed_tx_order(Some(0));

    store
        .upsert_input_notes(&[tracked_note.clone(), untracked_note.clone()])
        .await
        .unwrap();

    // Sanity: both notes are in the store as Consumed.
    let in_store = store.get_input_notes(NoteFilter::Consumed).await.unwrap();
    assert_eq!(in_store.len(), 2);

    // The reader keyed by the consumer should find the tracked note but not the untracked one.
    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer);

    let mut collected = Vec::new();
    while let Some(n) = reader.next().await.unwrap() {
        collected.push(n);
    }

    assert_eq!(
        collected.len(),
        1,
        "InputNoteReader should return externally-consumed notes when the consumer account is tracked",
    );
    assert_eq!(collected[0].id(), tracked_note.id());
    assert_eq!(collected[0].consumer_account(), Some(consumer));
}

#[tokio::test]
async fn input_note_reader_separates_notes_consumed_by_the_same_transaction() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    // Externally-consumed notes without metadata have no note id, and all three share a block
    // height and tx order, so only the details commitment separates them.
    let notes: Vec<_> = (0..3u32)
        .map(|index| {
            let mut note = create_consumed_external_input_note(index, 1, Some(consumer), None);
            note.set_consumed_tx_order(Some(0));
            note
        })
        .collect();
    store.upsert_input_notes(&notes).await.unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer);

    let mut collected = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        collected.push(note.details_commitment());
    }

    let mut expected: Vec<_> = notes.iter().map(InputNoteRecord::details_commitment).collect();
    expected.sort_by_key(Serializable::to_bytes);

    assert_eq!(collected, expected);
}

#[tokio::test]
async fn input_note_reader_reset_restarts_the_iteration() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let notes: Vec<_> = (0..3u32)
        .map(|index| create_consumed_input_note_with_consumer(consumer, index, index, 0))
        .collect();
    store.upsert_input_notes(&notes).await.unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer);

    let mut first_pass = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        first_pass.push(note.details_commitment());
    }
    assert_eq!(first_pass.len(), 3);

    reader.reset();

    let mut second_pass = Vec::new();
    while let Some(note) = reader.next().await.unwrap() {
        second_pass.push(note.details_commitment());
    }

    assert_eq!(first_pass, second_pass);
}

#[test]
fn input_note_cursor_is_none_for_a_note_that_is_not_consumed() {
    assert!(InputNoteCursor::from_record(&create_expected_input_note(0)).is_none());
}

#[tokio::test]
async fn input_note_after_ignores_a_cursor_before_the_block_range() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let note_at_1 = create_consumed_input_note_with_consumer(consumer, 0, 1, 0);
    // Follows the cursor but falls outside the range, so it must not be returned.
    let note_at_3 = create_consumed_input_note_with_consumer(consumer, 1, 3, 0);
    let note_at_5 = create_consumed_input_note_with_consumer(consumer, 2, 5, 0);
    store
        .upsert_input_notes(&[note_at_1.clone(), note_at_3, note_at_5.clone()])
        .await
        .unwrap();

    // A cursor before `block_start` selects nothing that the range does not already exclude, so
    // the first note in the range is returned.
    let cursor = InputNoteCursor::from_record(&note_at_1).unwrap();
    let note = store
        .get_input_note_after(
            NoteFilter::Consumed,
            consumer,
            Some(BlockNumber::from(5u32)),
            None,
            Some(cursor),
        )
        .await
        .unwrap()
        .expect("the range holds a note following the cursor");

    assert_eq!(note.details_commitment(), note_at_5.details_commitment());
}

#[tokio::test]
async fn input_note_reader_walks_every_note_of_a_long_history() {
    const BLOCKS: u32 = 8;
    const TXS_PER_BLOCK: u32 = 5;

    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let mut notes = Vec::new();
    for block in 1..=BLOCKS {
        for tx_order in 0..TXS_PER_BLOCK {
            let index = block * TXS_PER_BLOCK + tx_order;
            notes.push(create_consumed_input_note_with_consumer(consumer, index, block, tx_order));
        }
    }

    // Insert from the last note backwards, so a walk that leaned on insertion order would fail.
    let mut inserted = notes.clone();
    inserted.reverse();
    store.upsert_input_notes(&inserted).await.unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store, consumer);

    let mut expected: Vec<_> = notes.iter().map(consumption_key).collect();
    expected.sort();
    assert_eq!(expected.len(), usize::try_from(BLOCKS * TXS_PER_BLOCK).unwrap());

    // Equality against the full expected sequence rules out both a skipped and a repeated note.
    assert_eq!(walk(&mut reader).await, expected);
}

#[tokio::test]
async fn input_note_reader_only_returns_mid_iteration_inserts_after_the_cursor() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let at_2 = create_consumed_input_note_with_consumer(consumer, 60, 2, 0);
    store.upsert_input_notes(std::slice::from_ref(&at_2)).await.unwrap();

    let store: Arc<dyn Store> = Arc::new(store);
    let mut reader = InputNoteReader::new(store.clone(), consumer);

    let first = reader.next().await.unwrap().expect("the store holds one consumed note");
    assert_eq!(consumption_key(&first), consumption_key(&at_2));

    // One note lands before the cursor and one after it.
    let at_1 = create_consumed_input_note_with_consumer(consumer, 61, 1, 0);
    let at_3 = create_consumed_input_note_with_consumer(consumer, 62, 3, 0);
    store.upsert_input_notes(&[at_1, at_3.clone()]).await.unwrap();

    assert_eq!(walk(&mut reader).await, vec![consumption_key(&at_3)]);
}

#[tokio::test]
async fn input_note_after_keeps_a_cursor_at_the_start_of_the_block_range() {
    let store = create_test_store().await;
    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();

    let at_1 = create_consumed_input_note_with_consumer(consumer, 71, 1, 0);
    let first_at_3 = create_consumed_input_note_with_consumer(consumer, 72, 3, 0);
    let second_at_3 = create_consumed_input_note_with_consumer(consumer, 73, 3, 1);
    store
        .upsert_input_notes(&[at_1, first_at_3.clone(), second_at_3.clone()])
        .await
        .unwrap();

    // The cursor sits exactly at `block_start`, so it is the tighter bound: dropping it in favour
    // of the range would return the note the cursor points at all over again.
    let cursor = InputNoteCursor::from_record(&first_at_3).unwrap();
    let note = store
        .get_input_note_after(
            NoteFilter::Consumed,
            consumer,
            Some(BlockNumber::from(3u32)),
            None,
            Some(cursor),
        )
        .await
        .unwrap()
        .expect("the second note of block 3 follows the cursor");

    assert_eq!(note.details_commitment(), second_at_3.details_commitment());
}

// ORDERING TESTS (INPUT NOTES)
// ================================================================================================

#[tokio::test]
async fn consumed_input_notes_ordered_by_block_height_then_tx_order() {
    let store = create_test_store().await;

    // Create consumed notes at different block heights with tx_order set.
    let mut note_block3 = create_consumed_external_input_note(0, 3, None, None);
    let mut note_block1 = create_consumed_external_input_note(1, 1, None, None);
    let mut note_block2 = create_consumed_external_input_note(2, 2, None, None);
    note_block3.set_consumed_tx_order(Some(0));
    note_block1.set_consumed_tx_order(Some(1));
    note_block2.set_consumed_tx_order(Some(0));

    // Insert in non-sorted order.
    store
        .upsert_input_notes(&[note_block3.clone(), note_block1.clone(), note_block2.clone()])
        .await
        .unwrap();

    // Retrieve consumed notes — should be ordered by block_height ASC, tx_order ASC.
    let notes = store.get_input_notes(NoteFilter::Consumed).await.unwrap();
    assert_eq!(notes.len(), 3);
    assert_eq!(notes[0].id(), note_block1.id()); // block 1, tx_order 1
    assert_eq!(notes[1].id(), note_block2.id()); // block 2, tx_order 0
    assert_eq!(notes[2].id(), note_block3.id()); // block 3, tx_order 0
}

#[tokio::test]
async fn consumed_input_notes_same_block_ordered_by_tx_order() {
    let store = create_test_store().await;

    // All notes consumed at the same block height, different tx_order.
    let mut note_tx2 = create_consumed_external_input_note(10, 5, None, None);
    let mut note_tx0 = create_consumed_external_input_note(11, 5, None, None);
    let mut note_tx1 = create_consumed_external_input_note(12, 5, None, None);
    note_tx2.set_consumed_tx_order(Some(2));
    note_tx0.set_consumed_tx_order(Some(0));
    note_tx1.set_consumed_tx_order(Some(1));

    store
        .upsert_input_notes(&[note_tx2.clone(), note_tx0.clone(), note_tx1.clone()])
        .await
        .unwrap();

    let notes = store.get_input_notes(NoteFilter::Consumed).await.unwrap();
    assert_eq!(notes.len(), 3);
    assert_eq!(notes[0].id(), note_tx0.id()); // tx_order 0
    assert_eq!(notes[1].id(), note_tx1.id()); // tx_order 1
    assert_eq!(notes[2].id(), note_tx2.id()); // tx_order 2
}

#[tokio::test]
async fn consumed_input_notes_null_tx_order_sort_last_within_block() {
    let store = create_test_store().await;

    // Two notes at the same block: one with tx_order, one without (external consumption).
    let mut note_with_order = create_consumed_external_input_note(20, 5, None, None);
    let note_without_order = create_consumed_external_input_note(21, 5, None, None);
    note_with_order.set_consumed_tx_order(Some(0));

    store
        .upsert_input_notes(&[note_with_order.clone(), note_without_order.clone()])
        .await
        .unwrap();

    let notes = store.get_input_notes(NoteFilter::Consumed).await.unwrap();
    assert_eq!(notes.len(), 2);
    // Note with tx_order should come first (non-NULL sorts before NULL in ASC).
    assert_eq!(notes[0].id(), note_with_order.id());
    assert_eq!(notes[1].id(), note_without_order.id());
}

// SCRIPT ROOT FILTER TESTS
// ================================================================================================

#[tokio::test]
async fn input_notes_filtered_by_script_root() {
    let store = create_test_store().await;

    let swap_note_a = create_expected_input_note_with_script(0, StandardNote::SWAP.script());
    let swap_note_b = create_expected_input_note_with_script(1, StandardNote::SWAP.script());
    let p2id_note = create_expected_input_note_with_script(2, StandardNote::P2ID.script());

    store
        .upsert_input_notes(&[swap_note_a.clone(), swap_note_b.clone(), p2id_note.clone()])
        .await
        .unwrap();

    let notes = store
        .get_input_notes(NoteFilter::ScriptRoots(vec![StandardNote::P2ID.script().root()]))
        .await
        .unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].details_commitment(), p2id_note.details_commitment());

    let notes = store
        .get_input_notes(NoteFilter::ScriptRoots(vec![StandardNote::SWAP.script().root()]))
        .await
        .unwrap();
    let mut commitments: Vec<_> = notes.iter().map(InputNoteRecord::details_commitment).collect();
    commitments.sort();
    let mut expected_commitments =
        vec![swap_note_a.details_commitment(), swap_note_b.details_commitment()];
    expected_commitments.sort();
    assert_eq!(commitments, expected_commitments);

    let notes = store
        .get_input_notes(NoteFilter::ScriptRoots(vec![
            StandardNote::SWAP.script().root(),
            StandardNote::P2ID.script().root(),
        ]))
        .await
        .unwrap();
    assert_eq!(notes.len(), 3);

    let notes = store
        .get_input_notes(NoteFilter::ScriptRoots(vec![StandardNote::MINT.script().root()]))
        .await
        .unwrap();
    assert!(notes.is_empty());
}

#[tokio::test]
async fn output_notes_never_match_script_root_filter() {
    let store = create_test_store().await;

    let swap_note = create_expected_output_note_with_script(0, StandardNote::SWAP.script());

    let state_sync_update = StateSyncUpdate::from_parts(
        BlockNumber::from(0u32),
        PartialBlockchainUpdates::default(),
        NoteUpdateTracker::for_transaction_updates([], [], [swap_note.clone()]),
        TransactionUpdateTracker::default(),
        AccountUpdates::default(),
    );
    store.apply_state_sync(state_sync_update).await.unwrap();

    let notes = store.get_output_notes(NoteFilter::All).await.unwrap();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].id(), swap_note.id());

    // The `output_notes` table has no script root column, so the filter can never match.
    let notes = store
        .get_output_notes(NoteFilter::ScriptRoots(vec![StandardNote::SWAP.script().root()]))
        .await
        .unwrap();
    assert!(notes.is_empty());
}

// BATCH SCRIPT TESTS
// ================================================================================================

#[tokio::test]
async fn state_sync_stores_scripts_of_new_input_notes() {
    let store = create_test_store().await;

    // Two notes share the SWAP script, so the batch holds one entry per distinct root rather than
    // one per note. The multi-row upsert relies on that dedup: a root repeated inside a single
    // VALUES list would make ON CONFLICT DO UPDATE fail at runtime.
    let swap_a = create_expected_input_note_with_script(0, StandardNote::SWAP.script());
    let swap_b = create_expected_input_note_with_script(1, StandardNote::SWAP.script());
    let p2id = create_expected_input_note_with_script(2, StandardNote::P2ID.script());

    let notes = [swap_a, swap_b, p2id];

    // Applying the same update twice takes the insert branch and then the DO UPDATE branch.
    for _ in 0..2 {
        let state_sync_update = StateSyncUpdate::from_parts(
            BlockNumber::from(0u32),
            PartialBlockchainUpdates::default(),
            NoteUpdateTracker::for_transaction_updates(notes.clone(), [], []),
            TransactionUpdateTracker::default(),
            AccountUpdates::default(),
        );
        store.apply_state_sync(state_sync_update).await.unwrap();

        let swap_notes = store
            .get_input_notes(NoteFilter::ScriptRoots(vec![StandardNote::SWAP.script().root()]))
            .await
            .unwrap();
        assert_eq!(swap_notes.len(), 2);

        let p2id_notes = store
            .get_input_notes(NoteFilter::ScriptRoots(vec![StandardNote::P2ID.script().root()]))
            .await
            .unwrap();
        assert_eq!(p2id_notes.len(), 1);
        assert_eq!(p2id_notes[0].details().script().root(), StandardNote::P2ID.script().root());
    }
}

// UNSPENT NULLIFIER TESTS
// ================================================================================================

#[tokio::test]
async fn unspent_nullifiers_skip_notes_without_metadata() {
    let store = create_test_store().await;

    // An expected note without metadata has no nullifier, so its column is NULL.
    let without_metadata = create_expected_input_note(0);
    let with_metadata = create_expected_input_note_with_metadata(1);
    assert!(without_metadata.nullifier().is_none());

    store
        .upsert_input_notes(&[without_metadata, with_metadata.clone()])
        .await
        .unwrap();

    let nullifiers = store.get_unspent_input_note_nullifiers().await.unwrap();
    assert_eq!(nullifiers, vec![with_metadata.nullifier().unwrap()]);
}

#[tokio::test]
async fn unspent_nullifiers_exclude_consumed_notes() {
    let store = create_test_store().await;

    let consumer = AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap();
    let consumed_local = create_consumed_input_note_with_consumer(consumer, 0, 1, 0);
    let consumed_external = create_consumed_external_input_note(
        1,
        1,
        Some(consumer),
        Some(create_note_metadata(consumer, 1)),
    );
    let unspent = create_expected_input_note_with_metadata(2);

    // Both consumed notes carry a nullifier, so only the state filter can exclude them.
    assert!(consumed_local.nullifier().is_some());
    assert!(consumed_external.nullifier().is_some());

    store
        .upsert_input_notes(&[consumed_local, consumed_external, unspent.clone()])
        .await
        .unwrap();

    let nullifiers = store.get_unspent_input_note_nullifiers().await.unwrap();
    assert_eq!(nullifiers, vec![unspent.nullifier().unwrap()]);
}

#[test]
fn unspent_states_classify_every_note_state() {
    // Invalid notes sit here because they can't be consumed, so they are not offered as unspent
    // either.
    const SPENT_OR_UNCONSUMABLE: [u8; 4] = [
        InputNoteState::STATE_INVALID,
        InputNoteState::STATE_CONSUMED_AUTHENTICATED_LOCAL,
        InputNoteState::STATE_CONSUMED_UNAUTHENTICATED_LOCAL,
        InputNoteState::STATE_CONSUMED_EXTERNAL,
    ];

    for discriminant in 0..=u8::MAX {
        // The deserializer decides which bytes are real discriminants: an unused one hits the
        // catch-all arm and returns `InvalidValue`, while a real one gets past the discriminant
        // match and fails later on the payload a one-byte input doesn't carry. A new state whose
        // payload also reports `InvalidValue` would be skipped here instead of checked.
        if matches!(
            InputNoteState::read_from_bytes(&[discriminant]),
            Err(DeserializationError::InvalidValue(_))
        ) {
            continue;
        }

        assert!(
            InputNoteState::UNSPENT_STATES.contains(&discriminant)
                != SPENT_OR_UNCONSUMABLE.contains(&discriminant),
            "note state {discriminant} is in neither list or in both"
        );
    }
}

/// Attachment content is resolved during sync, after the record may already be stored, so a
/// state-only update has to persist it too. Attachments feed the note id, so a record that keeps
/// a stale set reconstructs to a different note than the on-chain one and becomes unconsumable.
#[tokio::test]
async fn input_note_state_update_persists_attachments() {
    let store = create_test_store().await;

    let note = create_expected_input_note(0);
    assert!(note.attachments().is_empty(), "the fixture starts without attachments");
    store.upsert_input_notes(std::slice::from_ref(&note)).await.unwrap();

    // The same note, now carrying the attachments a sync resolved for it. Attachments are not part
    // of `NoteDetails`, so the details commitment (the update key) is unchanged.
    let attachments = NoteAttachments::new(vec![NoteAttachment::with_word(
        NoteAttachmentScheme::new(42).unwrap(),
        Word::from([1u32, 2, 3, 4]),
    )])
    .unwrap();
    let updated = InputNoteRecord::new(
        note.details().clone(),
        attachments.clone(),
        Some(0),
        ExpectedNoteState {
            metadata: None,
            after_block_num: BlockNumber::from(0u32),
            tag: None,
        }
        .into(),
    );
    assert_eq!(updated.details_commitment(), note.details_commitment());

    let state_sync_update = StateSyncUpdate::from_parts(
        BlockNumber::from(0u32),
        PartialBlockchainUpdates::default(),
        NoteUpdateTracker::for_transaction_updates([], [updated], []),
        TransactionUpdateTracker::default(),
        AccountUpdates::default(),
    );
    store.apply_state_sync(state_sync_update).await.unwrap();

    let stored = store.get_input_notes(NoteFilter::All).await.unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(
        stored[0].attachments(),
        &attachments,
        "a state update must persist the attachments resolved for the note"
    );
}

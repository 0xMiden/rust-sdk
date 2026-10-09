use miden_client_core::store::{NoteUpdateTracker, NoteUpdateType};
use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{Note, NoteHeader, NoteId, NoteInclusionProof, Nullifier};
use miden_standards::note::NetworkAccountTarget;

use crate::ClientError;
use crate::rpc::domain::note::CommittedNote;
use crate::store::InputNoteRecord;
use crate::transaction::{TransactionRecord, TransactionStatus};

// NOTE CONSUMPTION
// ================================================================================================

/// A note consumption event observed on chain.
pub struct NoteConsumption {
    /// The nullifier of the consumed note.
    pub nullifier: Nullifier,
    /// The block number at which the note consumption was registered on chain.
    pub block_num: BlockNumber,
    /// The account ID of the consumer of the note. Will be set if the note was consumed by a
    /// transaction submitted outside this client by an account that is tracked locally. Otherwise,
    /// it will be `None`.
    pub external_consumer: Option<AccountId>,
}

// UPDATE FUNCTIONS
// ================================================================================================

/// Inserts the new public note data into the tracker. This function doesn't check the relevance of
/// the note, so it should only be used for notes that are guaranteed to be relevant to the client.
pub(crate) fn apply_new_public_note(
    tracker: &mut NoteUpdateTracker,
    mut public_note_data: InputNoteRecord,
    block_header: &BlockHeader,
) -> Result<(), ClientError> {
    public_note_data.block_header_received(block_header)?;
    tracker.insert_input_note(public_note_data, NoteUpdateType::Insert);

    Ok(())
}

/// Applies the necessary state transitions to the [`NoteUpdateTracker`] when a note is committed in
/// a block and returns whether the committed note is tracked as input note.
pub(crate) fn apply_committed_note_state_transitions(
    tracker: &mut NoteUpdateTracker,
    committed_note: &CommittedNote,
    block_header: &BlockHeader,
) -> Result<bool, ClientError> {
    let inclusion_proof = committed_note.inclusion_proof().clone();
    let metadata = *committed_note.metadata();
    let note_id = *committed_note.note_id();
    let attachments = committed_note.attachments().filter(|attachments| !attachments.is_empty());

    let is_tracked_as_input_note =
        if let Some(input_note_record) = tracker.get_input_note_by_id(note_id) {
            input_note_record.inclusion_proof_received(inclusion_proof.clone(), metadata)?;
            input_note_record.block_header_received(block_header)?;
            if let Some(attachments) = attachments {
                input_note_record.attachments_received(attachments.clone());
            }

            true
        } else if let Some(commitment) = tracker.expected_note_matching(note_id, &metadata) {
            // A metadata-less note whose id, with the committed metadata, equals this note id:
            // evolve it into a full record (its details commitment key is unchanged).
            let mut record = tracker
                .input_note_by_commitment(commitment)
                .expect("commitment was just matched against the tracked notes")
                .clone();
            record.inclusion_proof_received(inclusion_proof.clone(), metadata)?;
            record.block_header_received(block_header)?;
            if let Some(attachments) = attachments {
                record.attachments_received(attachments.clone());
            }

            // `InsertCommitted` so the now-known `note_id`/`nullifier` columns are persisted (a
            // full-row insert), while still being reported as a committed tracked note rather than
            // a newly-discovered one. The insert also registers the note in the id and nullifier
            // indices.
            tracker.insert_input_note(record, NoteUpdateType::InsertCommitted);

            true
        } else {
            false
        };

    try_commit_output_note(tracker, note_id, inclusion_proof)?;

    Ok(is_tracked_as_input_note)
}

/// Applies inclusion proofs from the transaction sync response to tracked output notes.
///
/// This transitions output notes from `Expected` to `Committed` state using the inclusion proofs
/// returned by `SyncTransactions`.
pub(crate) fn apply_output_note_inclusion_proofs(
    tracker: &mut NoteUpdateTracker,
    committed_notes: &[CommittedNote],
) -> Result<(), ClientError> {
    for committed_note in committed_notes {
        try_commit_output_note(
            tracker,
            *committed_note.note_id(),
            committed_note.inclusion_proof().clone(),
        )?;
    }
    Ok(())
}

/// Marks an erased note as consumed.
///
/// This handles notes that were erased due to same-batch note erasure: the note was created and
/// consumed within the same batch, so it never appeared in the block body. The `block_num` is the
/// block in which the creating transaction was committed.
///
/// The consumer account id is derived from the tracked input record's attachments (a
/// [`NetworkAccountTarget`], when present), not from the erased-note RPC stream, which delivers
/// only a [`NoteHeader`]. When no such attachment is present the consumer is left unknown.
pub(crate) fn mark_erased_note_as_consumed(
    tracker: &mut NoteUpdateTracker,
    note_header: &NoteHeader,
    block_num: BlockNumber,
) -> Result<(), ClientError> {
    let note_id = note_header.id();

    if let Some(output_note) = tracker.get_output_note_by_id(note_id)
        && output_note.is_inclusion_pending()
        && let Some(nullifier) = output_note.nullifier()
    {
        output_note.nullifier_received(nullifier, block_num)?;
    }

    // Read first, so that the record is marked as updated only when it changes.
    let unconsumed_input = tracker
        .input_note_by_id(note_id)
        .filter(|input_note| !input_note.is_consumed())
        .and_then(|input_note| {
            let consumer_account = NetworkAccountTarget::try_from(input_note.attachments())
                .ok()
                .map(|target| target.target_id());
            input_note.nullifier().map(|nullifier| (nullifier, consumer_account))
        });

    if let Some((nullifier, consumer_account)) = unconsumed_input {
        let input_note = tracker
            .get_input_note_by_id(note_id)
            .expect("input note was just found in the tracker");
        input_note.consumed_externally(nullifier, block_num, consumer_account)?;
        input_note.set_consumed_tx_order(Some(0));
    }

    Ok(())
}

/// Records `note` as consumed by `consumer`, as a
/// [`ConsumedExternal`](crate::store::InputNoteState::ConsumedExternal) input-note record.
///
/// No-op when the note is already tracked: recovery runs after transaction and nullifier
/// processing, so a tracked record's consumption has already been applied through
/// [`apply_note_consumption`].
pub(crate) fn insert_consumed_public_note(
    tracker: &mut NoteUpdateTracker,
    note: Note,
    consumer: AccountId,
    block_num: BlockNumber,
) -> Result<(), ClientError> {
    let note_id = note.id();
    if tracker.tracks_note(note_id) {
        return Ok(());
    }
    let nullifier = note.nullifier();
    // The consuming transaction belongs to this sync, so its nullifier must have a position in the
    // execution order; storing the record without one would break the ordering guarantees of
    // `InputNoteReader`.
    let order = tracker
        .get_nullifier_order(nullifier)
        .ok_or(ClientError::MissingConsumedNoteOrder(note_id))?;
    let mut record = InputNoteRecord::from(note);
    record.consumed_externally(nullifier, block_num, Some(consumer))?;
    record.set_consumed_tx_order(Some(order));
    tracker.insert_input_note(record, NoteUpdateType::Insert);
    Ok(())
}

/// Applies the necessary state transitions to the [`NoteUpdateTracker`] when a note is nullified in
/// a block.
///
/// For input note records two possible scenarios are considered:
/// 1. The note was being processed by a local transaction that just got committed.
/// 2. The note was consumed by a transaction not submitted by this client. This includes
///    consumption by untracked accounts as well as consumption by tracked accounts whose
///    transactions were submitted by other client instances. If a local transaction was processing
///    the note and it didn't get committed, the transaction should be discarded.
///
/// If the note is tracked as an output but not as an input (e.g. the client tracks both the sender
/// and the consumer), a new input record is created from the output details so the consumption
/// surfaces through `InputNoteReader`.
pub(crate) fn apply_note_consumption<'a>(
    tracker: &mut NoteUpdateTracker,
    consumption: &NoteConsumption,
    mut committed_transactions: impl Iterator<Item = &'a TransactionRecord>,
) -> Result<(), ClientError> {
    let nullifier = consumption.nullifier;
    let block_num = consumption.block_num;
    let external_consumer = consumption.external_consumer;
    let order = tracker.get_nullifier_order(nullifier);
    let input_present = tracker.has_input_note_with_nullifier(nullifier);

    if let Some(input_note_update) = tracker.get_input_note_update_by_nullifier(nullifier) {
        if let Some(consumer_transaction) = committed_transactions
            .find(|t| input_note_update.inner().consumer_transaction_id() == Some(&t.id))
        {
            // The note was being processed by a local transaction that just got committed
            if let TransactionStatus::Committed { block_number, .. } = consumer_transaction.status {
                input_note_update
                    .inner_mut()
                    .transaction_committed(consumer_transaction.id, block_number)?;
            }
        } else {
            // The note was consumed by a transaction not submitted by this client. If the consuming
            // account is tracked, external_consumer will be Some.
            input_note_update.inner_mut().consumed_externally(
                nullifier,
                block_num,
                external_consumer,
            )?;
        }
        input_note_update.inner_mut().set_consumed_tx_order(order);
    }

    if let Some(output_note_record) = tracker.get_output_note_by_nullifier(nullifier) {
        output_note_record.nullifier_received(nullifier, block_num)?;
    }

    if !input_present
        && let Some(consumer) = external_consumer
        && let Some(note_id) = tracker.output_note_id_by_nullifier(nullifier)
    {
        try_insert_consumed_input_from_output(tracker, note_id, consumer, block_num, order)?;
    }

    Ok(())
}

// HELPERS
// ================================================================================================

/// Builds a consumed input note record from a tracked output note and inserts it.
///
/// Used when an output note is consumed externally and the client should also surface it as a
/// consumed input — for example, when the same client tracks both the sender and the consumer of
/// the note. No-op if the input is already tracked, the output is not tracked, or the output cannot
/// be converted to a [`Note`].
fn try_insert_consumed_input_from_output(
    tracker: &mut NoteUpdateTracker,
    note_id: NoteId,
    consumer: AccountId,
    block_num: BlockNumber,
    consumed_tx_order: Option<u32>,
) -> Result<(), ClientError> {
    if tracker.has_input_note_with_id(note_id) {
        return Ok(());
    }
    let Some(output_note) = tracker.output_note_by_id(note_id) else {
        return Ok(());
    };
    let Ok(note) = Note::try_from(output_note.clone()) else {
        return Ok(());
    };

    let mut input_record = InputNoteRecord::from(note);
    let nullifier = input_record.nullifier().expect("record built from a full note has metadata");
    input_record.consumed_externally(nullifier, block_num, Some(consumer))?;
    input_record.set_consumed_tx_order(consumed_tx_order);
    tracker.insert_input_note(input_record, NoteUpdateType::Insert);
    Ok(())
}

/// If the note is tracked as an output note, transitions it to `Committed` with the given inclusion
/// proof. No-op if the note is not tracked.
fn try_commit_output_note(
    tracker: &mut NoteUpdateTracker,
    note_id: NoteId,
    inclusion_proof: NoteInclusionProof,
) -> Result<(), ClientError> {
    if let Some(output_note) = tracker.get_output_note_by_id(note_id) {
        output_note.inclusion_proof_received(inclusion_proof)?;
    }
    Ok(())
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::vec;

    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{
        NoteAssets,
        NoteAttachments,
        NoteDetails,
        NoteId,
        NoteMetadata,
        NoteRecipient,
        NoteStorage,
        NoteType,
        PartialNoteMetadata,
    };
    use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;
    use miden_protocol::transaction::TransactionId;
    use miden_protocol::utils::serde::{Deserializable, Serializable};
    use miden_protocol::{Felt, Word, ZERO};
    use miden_standards::note::StandardNote;

    use super::{NoteConsumption, NoteUpdateTracker, apply_note_consumption};
    use crate::store::InputNoteRecord;
    use crate::store::input_note_states::{NoteSubmissionData, ProcessingUnauthenticatedNoteState};
    use crate::transaction::TransactionRecord;

    // HELPERS
    // --------------------------------------------------------------------------------------------

    fn note_details(seed: u64) -> NoteDetails {
        let serial_number: Word = [Felt::new_unchecked(seed), ZERO, ZERO, ZERO].into();
        let recipient = NoteRecipient::new(
            serial_number,
            StandardNote::SWAP.script(),
            NoteStorage::new(vec![]).unwrap(),
        );
        NoteDetails::new(NoteAssets::new(vec![]).unwrap(), recipient)
    }

    fn note_metadata(sender: AccountId) -> NoteMetadata {
        NoteMetadata::new(
            PartialNoteMetadata::new(sender, NoteType::Public),
            &NoteAttachments::empty(),
        )
    }

    /// A metadata-bearing, not-yet-consumed note that can be externally consumed.
    fn processing_note(seed: u64, sender: AccountId) -> InputNoteRecord {
        let state = ProcessingUnauthenticatedNoteState {
            metadata: note_metadata(sender),
            after_block_num: BlockNumber::from(0u32),
            submission_data: NoteSubmissionData {
                submitted_at: Some(0),
                consumer_account: sender,
                consumer_transaction: TransactionId::from_raw(Word::default()),
            },
        };
        InputNoteRecord::new(note_details(seed), NoteAttachments::empty(), Some(0), state.into())
    }

    // TESTS
    // --------------------------------------------------------------------------------------------

    #[test]
    fn external_consumption_retains_note_id() {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let note = processing_note(3, sender);
        let id = note.id().expect("processing note has metadata");
        let nullifier = note.nullifier().expect("processing note has metadata");

        let mut tracker = NoteUpdateTracker::for_transaction_updates(vec![], vec![note], vec![]);
        assert_eq!(tracker.consumed_input_note_ids().count(), 0);

        // After external consumption the note must still be reported as consumed by its id.
        apply_note_consumption(
            &mut tracker,
            &NoteConsumption {
                nullifier,
                block_num: BlockNumber::from(5u32),
                external_consumer: None,
            },
            core::iter::empty::<&TransactionRecord>(),
        )
        .expect("external consumption should apply");

        let consumed: alloc::vec::Vec<NoteId> = tracker.consumed_input_note_ids().collect();
        assert_eq!(
            consumed,
            vec![id],
            "an externally consumed note must still be reported by its id"
        );
    }

    #[test]
    fn externally_consumed_note_id_survives_round_trip() {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let note = processing_note(12, sender);
        let id = note.id().expect("processing note has metadata");
        let nullifier = note.nullifier().expect("processing note has metadata");

        let mut tracker = NoteUpdateTracker::for_transaction_updates(vec![], vec![note], vec![]);

        // After external consumption the note must still be reported as consumed by its id via
        // `input_notes_by_id`, both in memory and after a serialization round trip.
        apply_note_consumption(
            &mut tracker,
            &NoteConsumption {
                nullifier,
                block_num: BlockNumber::from(5u32),
                external_consumer: None,
            },
            core::iter::empty::<&TransactionRecord>(),
        )
        .expect("external consumption should apply");

        // In memory the id is reported correctly.
        let before: alloc::vec::Vec<NoteId> = tracker.consumed_input_note_ids().collect();
        assert_eq!(before, vec![id]);

        // The retained id must survive a serialize/deserialize round trip.
        let bytes = tracker.to_bytes();
        let restored = NoteUpdateTracker::read_from_bytes(&bytes).expect("round-trip should work");
        let after: alloc::vec::Vec<NoteId> = restored.consumed_input_note_ids().collect();
        assert_eq!(
            after,
            vec![id],
            "the retained id of an externally consumed note must survive serialization"
        );
    }
}

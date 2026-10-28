use alloc::collections::BTreeMap;

use miden_client_core::store::{InputNoteUpdate, NoteUpdateType, NoteUpdates, OutputNoteUpdate};
use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{
    Note,
    NoteDetailsCommitment,
    NoteHeader,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    Nullifier,
};
use miden_protocol::transaction::TransactionId;
use miden_standards::note::NetworkAccountTarget;

use crate::ClientError;
use crate::rpc::domain::note::CommittedNote;
use crate::store::{InputNoteRecord, NoteRecordError, OutputNoteRecord};
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

// TRACKED NOTE
// ================================================================================================

/// A note record held by the [`NoteUpdateTracker`] and the store write it needs. The update type is
/// `None` when the record is loaded from the store and has no change to write.
#[derive(Clone, Debug)]
struct TrackedNote<R> {
    record: R,
    update_type: Option<NoteUpdateType>,
}

impl<R> TrackedNote<R> {
    fn new(record: R, update_type: Option<NoteUpdateType>) -> Self {
        Self { record, update_type }
    }

    fn inner(&self) -> &R {
        &self.record
    }

    /// Returns a mutable reference to the record. A record without an update type becomes an
    /// `Update`. A record with an update type keeps it.
    fn inner_mut(&mut self) -> &mut R {
        self.update_type.get_or_insert(NoteUpdateType::Update);
        &mut self.record
    }
}

// NOTE UPDATE TRACKER
// ================================================================================================

/// Tracks the note changes that a sync derives, before they become [`NoteUpdates`].
///
/// The tracker holds the notes that the client already stores and the new notes that the sync
/// finds. It lets state changes be applied to these notes, so a note that is already updated can be
/// updated again as new information is received.
#[derive(Clone, Debug, Default)]
pub(crate) struct NoteUpdateTracker {
    /// All tracked input note records, keyed by their details commitment. The details commitment is
    /// metadata-independent and therefore always available, including for metadata-less notes (e.g.
    /// expected notes imported from bare details, or future notes created by a transaction) that do
    /// not yet have a `NoteId`.
    input_notes: BTreeMap<NoteDetailsCommitment, TrackedNote<InputNoteRecord>>,
    /// All tracked output note records.
    output_notes: BTreeMap<NoteId, TrackedNote<OutputNoteRecord>>,
    /// Lookup index from nullifier to the details commitment of the input note. Only populated for
    /// metadata-bearing notes, as a metadata-less note has no nullifier.
    input_notes_by_nullifier: BTreeMap<Nullifier, NoteDetailsCommitment>,
    /// Lookup index from `NoteId` to the details commitment of the input note. Only populated for
    /// metadata-bearing notes, since a metadata-less note has no `NoteId`. Lets a note be found by
    /// its id even though `input_notes` is keyed by details commitment.
    input_notes_by_id: BTreeMap<NoteId, NoteDetailsCommitment>,
    /// Fast lookup map from nullifier to output note id.
    output_notes_by_nullifier: BTreeMap<Nullifier, NoteId>,
    /// Map from nullifier to its per-account position in the consuming transaction order.
    /// Nullifiers from the same account are in execution order; ordering across different accounts
    /// is not guaranteed.
    nullifier_order: BTreeMap<Nullifier, u32>,
}

impl NoteUpdateTracker {
    /// Creates a [`NoteUpdateTracker`] with already-tracked notes.
    pub(crate) fn new(
        input_notes: impl IntoIterator<Item = InputNoteRecord>,
        output_notes: impl IntoIterator<Item = OutputNoteRecord>,
    ) -> Self {
        let mut tracker = Self::default();
        for note in input_notes {
            tracker.insert_input_note(note, None);
        }
        for note in output_notes {
            tracker.insert_output_note(note);
        }

        tracker
    }

    /// Consumes the tracker and returns the notes that need a store write.
    pub(crate) fn into_note_updates(self) -> NoteUpdates {
        let input_notes = self
            .input_notes
            .into_values()
            .filter_map(|note| Some(InputNoteUpdate::new(note.record, note.update_type?)))
            .collect();
        let output_notes = self
            .output_notes
            .into_values()
            .filter_map(|note| Some(OutputNoteUpdate::new(note.record, note.update_type?)))
            .collect();

        NoteUpdates::new(input_notes, output_notes)
    }

    // GETTERS
    // --------------------------------------------------------------------------------------------

    /// Returns input and output note unspent nullifiers.
    pub(crate) fn unspent_nullifiers(&self) -> impl Iterator<Item = Nullifier> {
        let input_note_unspent_nullifiers = self
            .input_notes
            .values()
            .filter(|note| !note.inner().is_consumed())
            .filter_map(|note| note.inner().nullifier());

        let output_note_unspent_nullifiers = self
            .output_notes
            .values()
            .filter(|note| !note.inner().is_consumed())
            .filter_map(|note| note.inner().nullifier());

        input_note_unspent_nullifiers.chain(output_note_unspent_nullifiers)
    }

    /// Returns the block numbers containing input notes that remain unspent after this update.
    pub(crate) fn unspent_input_note_block_numbers(
        &self,
    ) -> impl Iterator<Item = BlockNumber> + '_ {
        self.input_notes
            .values()
            .filter(|note| !note.inner().is_consumed())
            .filter_map(|note| {
                note.inner().inclusion_proof().map(|proof| proof.location().block_num())
            })
    }

    /// Returns whether the note is already tracked as an input or output record.
    pub(crate) fn tracks_note(&self, note_id: NoteId) -> bool {
        self.input_notes_by_id.contains_key(&note_id) || self.output_notes.contains_key(&note_id)
    }

    /// Returns whether an input note with the provided ID is tracked.
    fn has_input_note_with_id(&self, note_id: NoteId) -> bool {
        self.input_notes_by_id.contains_key(&note_id)
    }

    /// Returns whether an input note with the provided nullifier is tracked.
    fn has_input_note_with_nullifier(&self, nullifier: Nullifier) -> bool {
        self.input_notes_by_nullifier.contains_key(&nullifier)
    }

    /// Returns the input note record with the provided ID if it exists.
    fn input_note_by_id(&self, note_id: NoteId) -> Option<&InputNoteRecord> {
        let commitment = self.input_notes_by_id.get(&note_id)?;
        self.input_notes.get(commitment).map(TrackedNote::inner)
    }

    /// Returns the input note record with the provided details commitment if it exists.
    fn input_note_by_commitment(
        &self,
        commitment: NoteDetailsCommitment,
    ) -> Option<&InputNoteRecord> {
        self.input_notes.get(&commitment).map(TrackedNote::inner)
    }

    /// Returns the output note record with the provided ID if it exists.
    fn output_note_by_id(&self, note_id: NoteId) -> Option<&OutputNoteRecord> {
        self.output_notes.get(&note_id).map(TrackedNote::inner)
    }

    /// Returns the ID of the output note with the provided nullifier if it exists.
    fn output_note_id_by_nullifier(&self, nullifier: Nullifier) -> Option<NoteId> {
        self.output_notes_by_nullifier.get(&nullifier).copied()
    }

    /// Returns the position of the given nullifier in the consuming transaction order, or `None` if
    /// it is not present.
    fn get_nullifier_order(&self, nullifier: Nullifier) -> Option<u32> {
        self.nullifier_order.get(&nullifier).copied()
    }

    /// Returns the details commitment of a tracked metadata-less note whose id, combined with
    /// `metadata`, equals `note_id`, i.e. the committed note is that imported note.
    fn expected_note_matching(
        &self,
        note_id: NoteId,
        metadata: &NoteMetadata,
    ) -> Option<NoteDetailsCommitment> {
        self.input_notes
            .iter()
            .filter(|(_, note)| note.inner().metadata().is_none())
            .map(|(commitment, _)| *commitment)
            .find(|commitment| NoteId::new(*commitment, metadata) == note_id)
    }

    // UPDATE METHODS
    // --------------------------------------------------------------------------------------------

    /// Refreshes the tracker with persisted input notes.
    ///
    /// Call this method before deriving state updates. Imported records can replace older records
    /// from the initial store snapshot. The records are already persisted, so they need no store
    /// update until their state changes.
    pub(crate) fn track_existing_input_notes(
        &mut self,
        notes: impl IntoIterator<Item = InputNoteRecord>,
    ) {
        for note in notes {
            self.insert_input_note(note, None);
        }
    }

    /// Appends nullifiers to the per-account ordered nullifier list.
    ///
    /// Nullifiers from the same account must be in execution order; ordering across different
    /// accounts is not guaranteed.
    pub(crate) fn extend_nullifiers(&mut self, nullifiers: impl IntoIterator<Item = Nullifier>) {
        for nullifier in nullifiers {
            let next_pos =
                u32::try_from(self.nullifier_order.len()).expect("nullifier count exceeds u32");
            self.nullifier_order.entry(nullifier).or_insert(next_pos);
        }
    }

    /// Releases notes that the discarded transaction was processing. Leaves all other notes
    /// unchanged.
    pub(crate) fn apply_transaction_discarded(
        &mut self,
        transaction_id: TransactionId,
    ) -> Result<(), NoteRecordError> {
        for note in self.input_notes.values_mut() {
            if note.record.transaction_discarded(transaction_id)? {
                // Mark the changed note for storage.
                note.inner_mut();
            }
        }

        Ok(())
    }

    /// Returns a mutable reference to the input note record with the provided ID if it exists.
    fn get_input_note_by_id(&mut self, note_id: NoteId) -> Option<&mut InputNoteRecord> {
        let commitment = self.input_notes_by_id.get(&note_id).copied()?;
        self.input_notes.get_mut(&commitment).map(TrackedNote::inner_mut)
    }

    /// Returns a mutable reference to the output note record with the provided ID if it exists.
    fn get_output_note_by_id(&mut self, note_id: NoteId) -> Option<&mut OutputNoteRecord> {
        self.output_notes.get_mut(&note_id).map(TrackedNote::inner_mut)
    }

    /// Returns a mutable reference to the tracked input note with the provided nullifier if it
    /// exists.
    fn get_input_note_update_by_nullifier(
        &mut self,
        nullifier: Nullifier,
    ) -> Option<&mut TrackedNote<InputNoteRecord>> {
        let commitment = self.input_notes_by_nullifier.get(&nullifier).copied()?;
        self.input_notes.get_mut(&commitment)
    }

    /// Returns a mutable reference to the output note record with the provided nullifier if it
    /// exists.
    fn get_output_note_by_nullifier(
        &mut self,
        nullifier: Nullifier,
    ) -> Option<&mut OutputNoteRecord> {
        let note_id = self.output_notes_by_nullifier.get(&nullifier).copied()?;
        self.output_notes.get_mut(&note_id).map(TrackedNote::inner_mut)
    }

    /// Inserts an input note. An update type of `None` means the note is already stored and has no
    /// change to write.
    fn insert_input_note(&mut self, note: InputNoteRecord, update_type: Option<NoteUpdateType>) {
        let tracked = TrackedNote::new(note, update_type);

        let commitment = tracked.inner().details_commitment();
        if let Some(note_id) = tracked.inner().id() {
            // A note with metadata supersedes any metadata-less record for the same commitment.
            let nullifier = tracked.inner().nullifier().expect("note with an id has metadata");
            self.input_notes_by_nullifier.insert(nullifier, commitment);
            self.input_notes_by_id.insert(note_id, commitment);
            self.input_notes.insert(commitment, tracked);
        } else if self.input_notes.get(&commitment).is_none_or(|n| n.inner().id().is_none()) {
            // No metadata yet means no `NoteId` and no computable nullifier. Track by details
            // commitment until a committed note supplies the metadata to evolve it, but do not
            // overwrite a metadata-bearing record that already supersedes it.
            self.input_notes.insert(commitment, tracked);
        }
    }

    /// Inserts an output note that is already stored.
    fn insert_output_note(&mut self, note: OutputNoteRecord) {
        let note_id = note.id();
        if let Some(nullifier) = note.nullifier() {
            self.output_notes_by_nullifier.insert(nullifier, note_id);
        }
        self.output_notes.insert(note_id, TrackedNote::new(note, None));
    }

    /// Inserts the new public note data into the tracker. This method doesn't check the relevance
    /// of the note, so it should only be used for notes that are guaranteed to be relevant to the
    /// client.
    pub(crate) fn apply_new_public_note(
        &mut self,
        mut public_note_data: InputNoteRecord,
        block_header: &BlockHeader,
    ) -> Result<(), ClientError> {
        public_note_data.block_header_received(block_header)?;
        self.insert_input_note(public_note_data, Some(NoteUpdateType::Insert));

        Ok(())
    }

    /// Applies the necessary state transitions to the [`NoteUpdateTracker`] when a note is
    /// committed in a block and returns whether the committed note is tracked as input note.
    pub(crate) fn apply_committed_note_state_transitions(
        &mut self,
        committed_note: &CommittedNote,
        block_header: &BlockHeader,
    ) -> Result<bool, ClientError> {
        let inclusion_proof = committed_note.inclusion_proof().clone();
        let metadata = *committed_note.metadata();
        let note_id = *committed_note.note_id();
        let attachments =
            committed_note.attachments().filter(|attachments| !attachments.is_empty());

        let is_tracked_as_input_note =
            if let Some(input_note_record) = self.get_input_note_by_id(note_id) {
                input_note_record.inclusion_received(
                    inclusion_proof.clone(),
                    metadata,
                    block_header,
                )?;
                if let Some(attachments) = attachments {
                    input_note_record.attachments_received(attachments.clone());
                }

                true
            } else if let Some(commitment) = self.expected_note_matching(note_id, &metadata) {
                // A metadata-less note whose id, with the committed metadata, equals this note id:
                // evolve it into a full record (its details commitment key is unchanged).
                let mut record = self
                    .input_note_by_commitment(commitment)
                    .expect("commitment was just matched against the tracked notes")
                    .clone();
                record.inclusion_received(inclusion_proof.clone(), metadata, block_header)?;
                if let Some(attachments) = attachments {
                    record.attachments_received(attachments.clone());
                }

                // `InsertCommitted` so the now-known `note_id`/`nullifier` columns are persisted (a
                // full-row insert), while still being reported as a committed tracked note rather
                // than a newly-discovered one. The insert also registers the note in the id and
                // nullifier indices.
                self.insert_input_note(record, Some(NoteUpdateType::InsertCommitted));

                true
            } else {
                false
            };

        self.try_commit_output_note(note_id, inclusion_proof)?;

        Ok(is_tracked_as_input_note)
    }

    /// Applies inclusion proofs from the transaction sync response to tracked output notes.
    ///
    /// This transitions output notes from `Expected` to `Committed` state using the inclusion
    /// proofs returned by `SyncTransactions`.
    pub(crate) fn apply_output_note_inclusion_proofs(
        &mut self,
        committed_notes: &[CommittedNote],
    ) -> Result<(), ClientError> {
        for committed_note in committed_notes {
            self.try_commit_output_note(
                *committed_note.note_id(),
                committed_note.inclusion_proof().clone(),
            )?;
        }
        Ok(())
    }

    /// Marks an erased note as consumed.
    ///
    /// This handles notes that were erased due to same-batch note erasure: the note was created and
    /// consumed within the same batch, so it never appeared in the block body. The `block_num` is
    /// the block in which the creating transaction was committed.
    ///
    /// The consumer account id is derived from the tracked input record's attachments (a
    /// [`NetworkAccountTarget`], when present), not from the erased-note RPC stream, which delivers
    /// only a [`NoteHeader`]. When no such attachment is present the consumer is left unknown.
    pub(crate) fn mark_erased_note_as_consumed(
        &mut self,
        note_header: &NoteHeader,
        block_num: BlockNumber,
    ) -> Result<(), ClientError> {
        let note_id = note_header.id();

        if let Some(output_note) = self.get_output_note_by_id(note_id)
            && output_note.is_inclusion_pending()
            && let Some(nullifier) = output_note.nullifier()
        {
            output_note.nullifier_received(nullifier, block_num)?;
        }

        // Read first, so that the record is marked as updated only when it changes.
        let unconsumed_input = self
            .input_note_by_id(note_id)
            .filter(|input_note| !input_note.is_consumed())
            .and_then(|input_note| {
                let consumer_account = NetworkAccountTarget::try_from(input_note.attachments())
                    .ok()
                    .map(|target| target.target_id());
                input_note.nullifier().map(|nullifier| (nullifier, consumer_account))
            });

        if let Some((nullifier, consumer_account)) = unconsumed_input {
            let input_note = self
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
    /// [`Self::apply_note_consumption`].
    pub(crate) fn insert_consumed_public_note(
        &mut self,
        note: Note,
        consumer: AccountId,
        block_num: BlockNumber,
    ) -> Result<(), ClientError> {
        let note_id = note.id();
        if self.tracks_note(note_id) {
            return Ok(());
        }
        let nullifier = note.nullifier();
        // The consuming transaction belongs to this sync, so its nullifier must have a position in
        // the execution order; storing the record without one would break the ordering guarantees
        // of `InputNoteReader`.
        let order = self
            .get_nullifier_order(nullifier)
            .ok_or(ClientError::MissingConsumedNoteOrder(note_id))?;
        let mut record = InputNoteRecord::from(note);
        record.consumed_externally(nullifier, block_num, Some(consumer))?;
        record.set_consumed_tx_order(Some(order));
        self.insert_input_note(record, Some(NoteUpdateType::Insert));
        Ok(())
    }

    /// Applies the necessary state transitions to the [`NoteUpdateTracker`] when a note is
    /// nullified in a block.
    ///
    /// For input note records two possible scenarios are considered:
    /// 1. The note was being processed by a local transaction that just got committed.
    /// 2. The note was consumed by a transaction not submitted by this client. This includes
    ///    consumption by untracked accounts as well as consumption by tracked accounts whose
    ///    transactions were submitted by other client instances. If a local transaction was
    ///    processing the note and it didn't get committed, the transaction should be discarded.
    ///
    /// If the note is tracked as an output but not as an input (e.g. the client tracks both the
    /// sender and the consumer), a new input record is created from the output details so the
    /// consumption surfaces through `InputNoteReader`.
    pub(crate) fn apply_note_consumption<'a>(
        &mut self,
        consumption: &NoteConsumption,
        mut committed_transactions: impl Iterator<Item = &'a TransactionRecord>,
    ) -> Result<(), ClientError> {
        let nullifier = consumption.nullifier;
        let block_num = consumption.block_num;
        let external_consumer = consumption.external_consumer;
        let order = self.get_nullifier_order(nullifier);
        let input_present = self.has_input_note_with_nullifier(nullifier);

        if let Some(input_note_update) = self.get_input_note_update_by_nullifier(nullifier) {
            if let Some(consumer_transaction) = committed_transactions
                .find(|t| input_note_update.inner().consumer_transaction_id() == Some(&t.id))
            {
                // The note was being processed by a local transaction that just got committed
                if let TransactionStatus::Committed { block_number, .. } =
                    consumer_transaction.status
                {
                    input_note_update
                        .inner_mut()
                        .transaction_committed(consumer_transaction.id, block_number)?;
                }
            } else {
                // The note was consumed by a transaction not submitted by this client. If the
                // consuming account is tracked, external_consumer will be Some.
                input_note_update.inner_mut().consumed_externally(
                    nullifier,
                    block_num,
                    external_consumer,
                )?;
            }
            input_note_update.inner_mut().set_consumed_tx_order(order);
        }

        if let Some(output_note_record) = self.get_output_note_by_nullifier(nullifier) {
            output_note_record.nullifier_received(nullifier, block_num)?;
        }

        if !input_present
            && let Some(consumer) = external_consumer
            && let Some(note_id) = self.output_note_id_by_nullifier(nullifier)
        {
            self.try_insert_consumed_input_from_output(note_id, consumer, block_num, order)?;
        }

        Ok(())
    }

    // PRIVATE HELPERS
    // --------------------------------------------------------------------------------------------

    /// Builds a consumed input note record from a tracked output note and inserts it.
    ///
    /// Used when an output note is consumed externally and the client should also surface it as a
    /// consumed input — for example, when the same client tracks both the sender and the consumer
    /// of the note. No-op if the input is already tracked, the output is not tracked, or the output
    /// cannot be converted to a [`Note`].
    fn try_insert_consumed_input_from_output(
        &mut self,
        note_id: NoteId,
        consumer: AccountId,
        block_num: BlockNumber,
        consumed_tx_order: Option<u32>,
    ) -> Result<(), ClientError> {
        if self.has_input_note_with_id(note_id) {
            return Ok(());
        }
        let Some(output_note) = self.output_note_by_id(note_id) else {
            return Ok(());
        };
        let Ok(note) = Note::try_from(output_note.clone()) else {
            return Ok(());
        };

        let mut input_record = InputNoteRecord::from(note);
        let nullifier =
            input_record.nullifier().expect("record built from a full note has metadata");
        input_record.consumed_externally(nullifier, block_num, Some(consumer))?;
        input_record.set_consumed_tx_order(consumed_tx_order);
        self.insert_input_note(input_record, Some(NoteUpdateType::Insert));
        Ok(())
    }

    /// If the note is tracked as an output note, transitions it to `Committed` with the given
    /// inclusion proof. No-op if the note is not tracked.
    fn try_commit_output_note(
        &mut self,
        note_id: NoteId,
        inclusion_proof: NoteInclusionProof,
    ) -> Result<(), ClientError> {
        if let Some(output_note) = self.get_output_note_by_id(note_id) {
            output_note.inclusion_proof_received(inclusion_proof)?;
        }
        Ok(())
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use alloc::vec;

    use miden_protocol::account::AccountId;
    use miden_protocol::block::{BlockHeader, BlockNumber};
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::{
        NoteAssets,
        NoteAttachments,
        NoteDetails,
        NoteId,
        NoteInclusionProof,
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

    use super::{NoteConsumption, NoteUpdateTracker, NoteUpdateType};
    use crate::store::input_note_states::{NoteSubmissionData, ProcessingUnauthenticatedNoteState};
    use crate::store::{InputNoteRecord, InputNoteState};
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

    /// A metadata-bearing note being processed by the local transaction `consumer_transaction`.
    fn processing_note_for(
        seed: u64,
        sender: AccountId,
        consumer_transaction: TransactionId,
    ) -> InputNoteRecord {
        let state = ProcessingUnauthenticatedNoteState {
            metadata: note_metadata(sender),
            after_block_num: BlockNumber::from(3u32),
            submission_data: NoteSubmissionData {
                submitted_at: Some(0),
                consumer_account: sender,
                consumer_transaction,
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

        let mut tracker = NoteUpdateTracker::new(vec![note], vec![]);
        assert_eq!(tracker.clone().into_note_updates().consumed_input_note_ids().count(), 0);

        // After external consumption the note must still be reported as consumed by its id.
        tracker
            .apply_note_consumption(
                &NoteConsumption {
                    nullifier,
                    block_num: BlockNumber::from(5u32),
                    external_consumer: None,
                },
                core::iter::empty::<&TransactionRecord>(),
            )
            .expect("external consumption should apply");

        let consumed: alloc::vec::Vec<NoteId> =
            tracker.into_note_updates().consumed_input_note_ids().collect();
        assert_eq!(
            consumed,
            vec![id],
            "an externally consumed note must still be reported by its id"
        );
    }

    #[test]
    fn discarded_transaction_releases_only_the_notes_it_was_processing() {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let discarded_tx =
            TransactionId::from_raw([Felt::new_unchecked(1), ZERO, ZERO, ZERO].into());
        let other_tx = TransactionId::from_raw([Felt::new_unchecked(2), ZERO, ZERO, ZERO].into());

        let released = processing_note_for(20, sender, discarded_tx);
        let released_id = released.id().unwrap();
        let released_nullifier = released.nullifier().unwrap();
        let expected_metadata = released.metadata().copied();
        let still_processing = processing_note_for(21, sender, other_tx);
        let still_processing_id = still_processing.id().unwrap();
        let consumed = processing_note_for(22, sender, discarded_tx);
        let consumed_id = consumed.id().unwrap();
        let consumed_nullifier = consumed.nullifier().unwrap();

        let mut tracker =
            NoteUpdateTracker::new(vec![released, still_processing, consumed], vec![]);

        // One of the discarded transaction's notes was consumed on chain by someone else.
        tracker
            .apply_note_consumption(
                &NoteConsumption {
                    nullifier: consumed_nullifier,
                    block_num: BlockNumber::from(5u32),
                    external_consumer: None,
                },
                core::iter::empty::<&TransactionRecord>(),
            )
            .unwrap();

        tracker.apply_transaction_discarded(discarded_tx).unwrap();

        let state_of = |tracker: &NoteUpdateTracker, id: NoteId| {
            tracker.input_note_by_id(id).map(|note| note.state().clone()).unwrap()
        };

        // The released note is expected again, keeps its metadata and its nullifier index, and is
        // flagged as a pending store write.
        let InputNoteState::Expected(state) = state_of(&tracker, released_id) else {
            panic!("the released note should be expected again");
        };
        assert_eq!(state.metadata, expected_metadata);
        assert_eq!(state.after_block_num, BlockNumber::from(3u32));
        assert_eq!(state.tag, expected_metadata.map(|metadata| metadata.tag()));
        assert!(tracker.unspent_nullifiers().any(|nullifier| nullifier == released_nullifier));

        // A note held by another transaction and a note consumed on chain are left alone.
        assert!(matches!(
            state_of(&tracker, still_processing_id),
            InputNoteState::ProcessingUnauthenticated(_)
        ));
        assert!(matches!(state_of(&tracker, consumed_id), InputNoteState::ConsumedExternal(_)));

        let note_updates = tracker.into_note_updates();
        assert_eq!(
            note_updates
                .input_notes()
                .iter()
                .find(|update| update.id() == Some(released_id))
                .map(|update| *update.update_type()),
            Some(NoteUpdateType::Update)
        );
        assert_eq!(note_updates.input_notes().len(), 2);
    }

    #[test]
    fn inclusion_received_preserves_processing_transaction() {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let mut note = processing_note(23, sender);
        let original = note.clone();
        let metadata = *note.metadata().unwrap();
        let root = note.id().unwrap().as_word();
        let proof =
            NoteInclusionProof::new(4.into(), 0, SparseMerklePath::from_parts(0, vec![]).unwrap())
                .unwrap();
        let header = BlockHeader::mock(4, None, Some(root), &[]);
        let wrong_metadata = NoteMetadata::new(
            PartialNoteMetadata::new(sender, NoteType::Private),
            &NoteAttachments::empty(),
        );

        for (header, metadata) in [
            (BlockHeader::mock(5, None, Some(root), &[]), metadata),
            (BlockHeader::mock(4, None, Some(Word::default()), &[]), metadata),
            (header.clone(), wrong_metadata),
        ] {
            assert!(note.inclusion_received(proof.clone(), metadata, &header).is_err());
            assert_eq!(note, original);
        }

        assert!(note.inclusion_received(proof.clone(), metadata, &header).unwrap());
        let InputNoteState::ProcessingAuthenticated(state) = note.state() else {
            panic!("the note must remain processing");
        };
        let InputNoteState::ProcessingUnauthenticated(original_state) = original.state() else {
            unreachable!();
        };
        assert_eq!(state.submission_data, original_state.submission_data);
        assert_eq!(state.inclusion_proof, proof);
        assert_eq!(InputNoteRecord::read_from_bytes(&note.to_bytes()).unwrap(), note);
        assert!(!note.inclusion_received(proof, metadata, &header).unwrap());
    }
}

use alloc::collections::BTreeMap;

use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{
    Note,
    NoteAttachments,
    NoteDetailsCommitment,
    NoteHeader,
    NoteId,
    NoteInclusionProof,
    NoteMetadata,
    Nullifier,
};
use miden_standards::note::NetworkAccountTarget;
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use crate::ClientError;
use crate::rpc::domain::note::CommittedNote;
use crate::store::{InputNoteRecord, OutputNoteRecord};
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
    /// transaction submitted outside this client by an account that is tracked locally.
    /// Otherwise, it will be `None`.
    pub external_consumer: Option<AccountId>,
}

// NOTE UPDATE
// ================================================================================================

/// Represents the possible types of updates that can be applied to a note in a
/// [`NoteUpdateTracker`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NoteUpdateType {
    /// Indicates that the note was already tracked but it was not updated.
    None = 0,
    /// Indicates that the note is new and should be inserted in the store.
    Insert = 1,
    /// Indicates that the note was already tracked and should be updated.
    Update = 2,
    /// Indicates that a previously-tracked metadata-less (`Expected`) note has just been committed.
    /// It must be persisted as a full-row insert (like [`Self::Insert`]) so its now-known `note_id`
    /// and `nullifier` columns are written, but for reporting it is a *committed* tracked note —
    /// not a newly-discovered one — so it is summarized under committed notes, not new notes.
    InsertCommitted = 3,
}

impl NoteUpdateType {
    /// Whether this update carries a pending store write, as opposed to a note that was merely
    /// loaded as already-tracked context ([`Self::None`]). True for [`Self::Insert`],
    /// [`Self::Update`], and [`Self::InsertCommitted`].
    pub fn is_modified(self) -> bool {
        matches!(self, Self::Insert | Self::Update | Self::InsertCommitted)
    }
}

impl TryFrom<u8> for NoteUpdateType {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(NoteUpdateType::None),
            1 => Ok(NoteUpdateType::Insert),
            2 => Ok(NoteUpdateType::Update),
            3 => Ok(NoteUpdateType::InsertCommitted),
            other => Err(other),
        }
    }
}

/// Represents the possible states of an input note record in a [`NoteUpdateTracker`].
#[derive(Clone, Debug, PartialEq)]
pub struct InputNoteUpdate {
    /// Input note being updated.
    note: InputNoteRecord,
    /// Type of the note update.
    update_type: NoteUpdateType,
}

impl InputNoteUpdate {
    /// Creates a new [`InputNoteUpdate`] with the provided note with a `None` update type.
    fn new_none(note: InputNoteRecord) -> Self {
        Self { note, update_type: NoteUpdateType::None }
    }

    /// Creates a new [`InputNoteUpdate`] with the provided note with an `Insert` update type.
    fn new_insert(note: InputNoteRecord) -> Self {
        Self {
            note,
            update_type: NoteUpdateType::Insert,
        }
    }

    /// Creates a new [`InputNoteUpdate`] with the provided note with an `Update` update type.
    fn new_update(note: InputNoteRecord) -> Self {
        Self {
            note,
            update_type: NoteUpdateType::Update,
        }
    }

    /// Creates a new [`InputNoteUpdate`] for a previously-tracked expected note that has just been
    /// committed (see [`NoteUpdateType::InsertCommitted`]).
    fn new_insert_committed(note: InputNoteRecord) -> Self {
        Self {
            note,
            update_type: NoteUpdateType::InsertCommitted,
        }
    }

    /// Returns a reference the inner note record.
    pub fn inner(&self) -> &InputNoteRecord {
        &self.note
    }

    /// Returns a mutable reference to the inner note record. If the update type is `None` or
    /// `Update`, it will be set to `Update`; insert-typed updates keep their type.
    fn inner_mut(&mut self) -> &mut InputNoteRecord {
        self.update_type = match self.update_type {
            NoteUpdateType::None | NoteUpdateType::Update => NoteUpdateType::Update,
            NoteUpdateType::Insert => NoteUpdateType::Insert,
            NoteUpdateType::InsertCommitted => NoteUpdateType::InsertCommitted,
        };

        &mut self.note
    }

    /// Returns the type of the note update.
    pub fn update_type(&self) -> &NoteUpdateType {
        &self.update_type
    }

    /// Returns the identifier of the inner note. Returns `None` when the underlying
    /// [`InputNoteRecord`] has no metadata (see [`InputNoteRecord::id`]).
    pub fn id(&self) -> Option<NoteId> {
        self.note.id()
    }

    /// Returns the per-account position of the consuming transaction within the account's
    /// execution chain for the block. `None` for non-consumed notes or when the order has not
    /// been determined yet.
    pub fn consumed_tx_order(&self) -> Option<u32> {
        self.note.state().consumed_tx_order()
    }
}

/// Represents the possible states of an output note record in a [`NoteUpdateTracker`].
#[derive(Clone, Debug, PartialEq)]
pub struct OutputNoteUpdate {
    /// Output note being updated.
    note: OutputNoteRecord,
    /// Type of the note update.
    update_type: NoteUpdateType,
}

impl OutputNoteUpdate {
    /// Creates a new [`OutputNoteUpdate`] with the provided note with a `None` update type.
    fn new_none(note: OutputNoteRecord) -> Self {
        Self { note, update_type: NoteUpdateType::None }
    }

    /// Creates a new [`OutputNoteUpdate`] with the provided note with an `Insert` update type.
    fn new_insert(note: OutputNoteRecord) -> Self {
        Self {
            note,
            update_type: NoteUpdateType::Insert,
        }
    }

    /// Creates a new [`OutputNoteUpdate`] with the provided note with an `Update` update type.
    fn new_update(note: OutputNoteRecord) -> Self {
        Self {
            note,
            update_type: NoteUpdateType::Update,
        }
    }

    /// Returns a reference the inner note record.
    pub fn inner(&self) -> &OutputNoteRecord {
        &self.note
    }

    /// Returns a mutable reference to the inner note record. If the update type is `None` or
    /// `Update`, it will be set to `Update`.
    fn inner_mut(&mut self) -> &mut OutputNoteRecord {
        self.update_type = match self.update_type {
            NoteUpdateType::None | NoteUpdateType::Update => NoteUpdateType::Update,
            // Output notes are never assigned `InsertCommitted` (it is input-note specific), but
            // the match must be exhaustive; treat it as an insert.
            NoteUpdateType::Insert | NoteUpdateType::InsertCommitted => NoteUpdateType::Insert,
        };

        &mut self.note
    }

    /// Returns the type of the note update.
    pub fn update_type(&self) -> &NoteUpdateType {
        &self.update_type
    }

    /// Returns the identifier of the inner note.
    pub fn id(&self) -> NoteId {
        self.note.id()
    }
}

// NOTE UPDATE TRACKER
// ================================================================================================

/// Contains note changes to apply to the store.
///
/// This includes new notes that have been created and existing notes that have been updated. The
/// tracker also lets state changes be applied to the contained notes, this allows for already
/// updated notes to be further updated as new information is received.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NoteUpdateTracker {
    /// All new and updated input note records to be upserted in the store, keyed by their details
    /// commitment. The details commitment is metadata-independent and therefore always available,
    /// including for metadata-less notes (e.g. expected notes imported from bare details, or
    /// future notes created by a transaction) that do not yet have a `NoteId`.
    input_notes: BTreeMap<NoteDetailsCommitment, InputNoteUpdate>,
    /// A map of updated output note records to be upserted in the store.
    output_notes: BTreeMap<NoteId, OutputNoteUpdate>,
    /// Lookup index from nullifier to the details commitment of the input note. Only populated for
    /// metadata-bearing notes, as a metadata-less note has no nullifier.
    input_notes_by_nullifier: BTreeMap<Nullifier, NoteDetailsCommitment>,
    /// Lookup index from `NoteId` to the details commitment of the input note. Only populated for
    /// metadata-bearing notes. The entry is retained after a note loses its metadata (e.g. once it
    /// is externally consumed), so the note can still be reported by its id.
    input_notes_by_id: BTreeMap<NoteId, NoteDetailsCommitment>,
    /// Fast lookup map from nullifier to output note id.
    output_notes_by_nullifier: BTreeMap<Nullifier, NoteId>,
    /// Map from nullifier to its per-account position in the consuming transaction order.
    /// Nullifiers from the same account are in execution order; ordering across different
    /// accounts is not guaranteed.
    nullifier_order: BTreeMap<Nullifier, u32>,
}

impl NoteUpdateTracker {
    /// Creates a [`NoteUpdateTracker`] with already-tracked notes.
    pub fn new(
        input_notes: impl IntoIterator<Item = InputNoteRecord>,
        output_notes: impl IntoIterator<Item = OutputNoteRecord>,
    ) -> Self {
        let mut tracker = Self::default();
        for note in input_notes {
            tracker.insert_input_note(note, NoteUpdateType::None);
        }
        for note in output_notes {
            tracker.insert_output_note(note, NoteUpdateType::None);
        }

        tracker
    }

    /// Creates a [`NoteUpdateTracker`] for updates related to transactions.
    ///
    /// A transaction can:
    ///
    /// - Create input notes
    /// - Update existing input notes (by consuming them)
    /// - Create output notes
    pub fn for_transaction_updates(
        new_input_notes: impl IntoIterator<Item = InputNoteRecord>,
        updated_input_notes: impl IntoIterator<Item = InputNoteRecord>,
        new_output_notes: impl IntoIterator<Item = OutputNoteRecord>,
    ) -> Self {
        let mut tracker = Self::default();

        for note in new_input_notes {
            tracker.insert_input_note(note, NoteUpdateType::Insert);
        }

        for note in updated_input_notes {
            tracker.insert_input_note(note, NoteUpdateType::Update);
        }

        for note in new_output_notes {
            tracker.insert_output_note(note, NoteUpdateType::Insert);
        }

        tracker
    }

    // GETTERS
    // --------------------------------------------------------------------------------------------

    /// Returns all input note records that have been updated.
    ///
    /// This may include:
    /// - New notes that have been created that should be inserted.
    /// - Existing tracked notes that should be updated.
    ///
    /// Metadata-less expected notes (e.g. future notes created by a transaction, such as swap
    /// payback notes) are included as well: they have no `NoteId` yet but must still be persisted
    /// and have their tags registered. The `update_type` filter ensures notes merely loaded as
    /// already-tracked context (`NoteUpdateType::None`) are not re-emitted.
    pub fn updated_input_notes(&self) -> impl Iterator<Item = &InputNoteUpdate> {
        self.input_notes.values().filter(|note| note.update_type.is_modified())
    }

    /// Returns the ids of updated input notes that are now consumed. An externally consumed note
    /// carries no metadata, so `InputNoteRecord::id` is `None`. The `input_notes_by_id` index is
    /// consulted instead, since it retains the id of a note that has lost its metadata.
    pub fn consumed_input_note_ids(&self) -> impl Iterator<Item = NoteId> + '_ {
        self.input_notes_by_id.iter().filter_map(|(note_id, commitment)| {
            let update = self.input_notes.get(commitment)?;
            (update.update_type.is_modified() && update.inner().is_consumed()).then_some(*note_id)
        })
    }

    /// `NoteId`s of every input + output note that transitioned to a consumed state this sync.
    /// These are confirmed consumptions reflected in the tracker, not raw nullifier-prefix hits.
    pub fn consumed_note_ids(&self) -> impl Iterator<Item = NoteId> + '_ {
        let output = self.output_notes.iter().filter_map(|(note_id, update)| {
            (update.update_type.is_modified() && update.inner().is_consumed()).then_some(*note_id)
        });
        self.consumed_input_note_ids().chain(output)
    }

    /// Returns all output note records that have been updated.
    ///
    /// This may include:
    /// - New notes that have been created that should be inserted.
    /// - Existing tracked notes that should be updated.
    pub fn updated_output_notes(&self) -> impl Iterator<Item = &OutputNoteUpdate> {
        self.output_notes.values().filter(|note| note.update_type.is_modified())
    }

    /// Returns whether no new note-related information has been retrieved.
    pub fn is_empty(&self) -> bool {
        self.input_notes.is_empty() && self.output_notes.is_empty()
    }

    /// Returns input and output note unspent nullifiers.
    pub fn unspent_nullifiers(&self) -> impl Iterator<Item = Nullifier> {
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

    /// Appends nullifiers to the per-account ordered nullifier list.
    ///
    /// Nullifiers from the same account must be in execution order; ordering across different
    /// accounts is not guaranteed.
    pub fn extend_nullifiers(&mut self, nullifiers: impl IntoIterator<Item = Nullifier>) {
        for nullifier in nullifiers {
            let next_pos =
                u32::try_from(self.nullifier_order.len()).expect("nullifier count exceeds u32");
            self.nullifier_order.entry(nullifier).or_insert(next_pos);
        }
    }

    // UPDATE METHODS
    // --------------------------------------------------------------------------------------------

    /// Inserts the new public note data into the tracker. This method doesn't check the relevance
    /// of the note, so it should only be used for notes that are guaranteed to be relevant to the
    /// client.
    pub(crate) fn apply_new_public_note(
        &mut self,
        mut public_note_data: InputNoteRecord,
        block_header: &BlockHeader,
    ) -> Result<(), ClientError> {
        public_note_data.block_header_received(block_header)?;
        self.insert_input_note(public_note_data, NoteUpdateType::Insert);

        Ok(())
    }

    /// Applies the necessary state transitions to the [`NoteUpdateTracker`] when a note is
    /// committed in a block and returns whether the committed note is tracked as input note.
    pub(crate) fn apply_committed_note_state_transitions(
        &mut self,
        committed_note: &CommittedNote,
        block_header: &BlockHeader,
        attachments: Option<&NoteAttachments>,
    ) -> Result<bool, ClientError> {
        let inclusion_proof = committed_note.inclusion_proof().clone();
        let metadata = *committed_note.metadata();
        let note_id = *committed_note.note_id();

        let is_tracked_as_input_note = if let Some(input_note_record) =
            self.get_input_note_by_id(note_id)
        {
            input_note_record.inclusion_proof_received(inclusion_proof.clone(), metadata)?;
            input_note_record.block_header_received(block_header)?;
            if let Some(attachments) = attachments {
                input_note_record.set_attachments(attachments.clone());
            }

            true
        } else if let Some(commitment) = self.expected_note_matching(note_id, &metadata) {
            // A metadata-less note whose id, with the committed metadata, equals this note id:
            // evolve it into a full record in place (its details commitment key is unchanged).
            let nullifier = {
                let update = self
                    .input_notes
                    .get_mut(&commitment)
                    .expect("commitment was just matched against the tracked notes");
                let record = &mut update.note;
                record.inclusion_proof_received(inclusion_proof.clone(), metadata)?;
                record.block_header_received(block_header)?;
                if let Some(attachments) = attachments {
                    record.set_attachments(attachments.clone());
                }

                // `InsertCommitted` so the now-known `note_id`/`nullifier` columns are persisted
                // (a full-row insert), while still being reported as a committed tracked note
                // rather than a newly-discovered one.
                update.update_type = NoteUpdateType::InsertCommitted;
                record.nullifier().expect("note with an id has metadata")
            };

            // The note now has metadata, so register it in the id and nullifier indices.
            self.input_notes_by_nullifier.insert(nullifier, commitment);
            self.input_notes_by_id.insert(note_id, commitment);

            true
        } else {
            false
        };

        self.try_commit_output_note(note_id, inclusion_proof)?;

        Ok(is_tracked_as_input_note)
    }

    /// Applies inclusion proofs from the transaction sync response to tracked output notes.
    ///
    /// This transitions output notes from `Expected` to `Committed` state using the
    /// inclusion proofs returned by `SyncTransactions`.
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
    /// This handles notes that were erased due to same-batch note erasure: the note was
    /// created and consumed within the same batch, so it never appeared in the block body.
    /// The `block_num` is the block in which the creating transaction was committed.
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
            && !output_note.is_consumed()
            && !output_note.is_committed()
            && let Some(nullifier) = output_note.nullifier()
        {
            output_note.nullifier_received(nullifier, block_num)?;
        }

        if let Some(commitment) = self.input_notes_by_id.get(&note_id).copied()
            && let Some(input_note_update) = self.input_notes.get_mut(&commitment)
            && !input_note_update.inner().is_consumed()
            && let Some(nullifier) = input_note_update.inner().nullifier()
        {
            let consumer_account =
                NetworkAccountTarget::try_from(input_note_update.inner().attachments())
                    .ok()
                    .map(|target| target.target_id());
            input_note_update.inner_mut().consumed_externally(
                nullifier,
                block_num,
                consumer_account,
            )?;
            input_note_update.inner_mut().set_consumed_tx_order(Some(0));
        }

        Ok(())
    }

    /// Builds a consumed input note record from a tracked output note and inserts it.
    ///
    /// Used when an output note is consumed externally and the client should also surface
    /// it as a consumed input — for example, when the same client tracks both the sender
    /// and the consumer of the note. No-op if the input is already tracked, the output is
    /// not tracked, or the output cannot be converted to a [`Note`].
    fn try_insert_consumed_input_from_output(
        &mut self,
        note_id: NoteId,
        consumer: AccountId,
        block_num: BlockNumber,
        consumed_tx_order: Option<u32>,
    ) -> Result<(), ClientError> {
        if self.input_notes_by_id.contains_key(&note_id) {
            return Ok(());
        }
        let Some(output_note) = self.output_notes.get(&note_id) else {
            return Ok(());
        };
        let Ok(note) = Note::try_from(output_note.inner().clone()) else {
            return Ok(());
        };

        let mut input_record = InputNoteRecord::from(note);
        let nullifier =
            input_record.nullifier().expect("record built from a full note has metadata");
        input_record.consumed_externally(nullifier, block_num, Some(consumer))?;
        input_record.set_consumed_tx_order(consumed_tx_order);
        self.insert_input_note(input_record, NoteUpdateType::Insert);
        Ok(())
    }

    /// If the note is tracked as an output note, transitions it to `Committed` with the
    /// given inclusion proof. No-op if the note is not tracked.
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
        let input_present = self.input_notes_by_nullifier.contains_key(&nullifier);

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
                // The note was consumed by a transaction not submitted by this client.
                // If the consuming account is tracked, external_consumer will be Some.
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
            && let Some(note_id) = self.output_notes_by_nullifier.get(&nullifier).copied()
        {
            self.try_insert_consumed_input_from_output(note_id, consumer, block_num, order)?;
        }

        Ok(())
    }

    // PRIVATE HELPERS
    // --------------------------------------------------------------------------------------------

    /// Returns the position of the given nullifier in the consuming transaction order, or `None`
    /// if it is not present.
    fn get_nullifier_order(&self, nullifier: Nullifier) -> Option<u32> {
        self.nullifier_order.get(&nullifier).copied()
    }

    /// Returns a mutable reference to the input note record with the provided ID if it exists.
    fn get_input_note_by_id(&mut self, note_id: NoteId) -> Option<&mut InputNoteRecord> {
        let commitment = self.input_notes_by_id.get(&note_id).copied()?;
        self.input_notes.get_mut(&commitment).map(InputNoteUpdate::inner_mut)
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
            .filter(|(_, update)| update.inner().metadata().is_none())
            .map(|(commitment, _)| *commitment)
            .find(|commitment| NoteId::new(*commitment, metadata) == note_id)
    }

    /// Returns a mutable reference to the output note record with the provided ID if it exists.
    fn get_output_note_by_id(&mut self, note_id: NoteId) -> Option<&mut OutputNoteRecord> {
        self.output_notes.get_mut(&note_id).map(OutputNoteUpdate::inner_mut)
    }

    /// Returns a mutable reference to the input note update with the provided nullifier if it
    /// exists.
    fn get_input_note_update_by_nullifier(
        &mut self,
        nullifier: Nullifier,
    ) -> Option<&mut InputNoteUpdate> {
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
        self.output_notes.get_mut(&note_id).map(OutputNoteUpdate::inner_mut)
    }

    /// Insert an input note update
    fn insert_input_note(&mut self, note: InputNoteRecord, update_type: NoteUpdateType) {
        let update = match update_type {
            NoteUpdateType::None => InputNoteUpdate::new_none(note),
            NoteUpdateType::Insert => InputNoteUpdate::new_insert(note),
            NoteUpdateType::Update => InputNoteUpdate::new_update(note),
            NoteUpdateType::InsertCommitted => InputNoteUpdate::new_insert_committed(note),
        };

        let commitment = update.inner().details_commitment();
        if let Some(note_id) = update.inner().id() {
            // A note with metadata supersedes any metadata-less record for the same commitment.
            let nullifier = update.inner().nullifier().expect("note with an id has metadata");
            self.input_notes_by_nullifier.insert(nullifier, commitment);
            self.input_notes_by_id.insert(note_id, commitment);
            self.input_notes.insert(commitment, update);
        } else if self.input_notes.get(&commitment).is_none_or(|u| u.inner().id().is_none()) {
            // No metadata yet means no `NoteId` and no computable nullifier. Track by details
            // commitment until a committed note supplies the metadata to evolve it, but do not
            // overwrite a metadata-bearing record that already supersedes it.
            self.input_notes.insert(commitment, update);
        }
    }

    /// Insert an output note update
    fn insert_output_note(&mut self, note: OutputNoteRecord, update_type: NoteUpdateType) {
        let note_id = note.id();
        if let Some(nullifier) = note.nullifier() {
            self.output_notes_by_nullifier.insert(nullifier, note_id);
        }
        let update = match update_type {
            NoteUpdateType::None => OutputNoteUpdate::new_none(note),
            NoteUpdateType::Update => OutputNoteUpdate::new_update(note),
            // Output notes are never assigned `InsertCommitted`; treat it as an insert for
            // exhaustiveness.
            NoteUpdateType::Insert | NoteUpdateType::InsertCommitted => {
                OutputNoteUpdate::new_insert(note)
            },
        };
        self.output_notes.insert(note_id, update);
    }
}

// SERIALIZATION
// ================================================================================================

impl Serializable for NoteUpdateType {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write_u8(*self as u8);
    }
}

impl Deserializable for NoteUpdateType {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        NoteUpdateType::try_from(source.read_u8()?).map_err(|val| {
            DeserializationError::InvalidValue(format!("invalid note update type: {val}"))
        })
    }
}

impl Serializable for InputNoteUpdate {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.note.write_into(target);
        self.update_type.write_into(target);
    }
}

impl Deserializable for InputNoteUpdate {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let note = InputNoteRecord::read_from(source)?;
        let update_type = NoteUpdateType::read_from(source)?;
        Ok(Self { note, update_type })
    }
}

impl Serializable for OutputNoteUpdate {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.note.write_into(target);
        self.update_type.write_into(target);
    }
}

impl Deserializable for OutputNoteUpdate {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let note = OutputNoteRecord::read_from(source)?;
        let update_type = NoteUpdateType::read_from(source)?;
        Ok(Self { note, update_type })
    }
}

impl Serializable for NoteUpdateTracker {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        // These indices retain entries for metadata-less notes (e.g. externally-consumed ones)
        // whose id and nullifier can no longer be recovered from the records, so they are
        // serialized to preserve them across a round trip.
        self.input_notes.write_into(target);
        self.output_notes.write_into(target);
        self.nullifier_order.write_into(target);
        self.input_notes_by_id.write_into(target);
        self.input_notes_by_nullifier.write_into(target);
    }
}

impl Deserializable for NoteUpdateTracker {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let input_notes = BTreeMap::<NoteDetailsCommitment, InputNoteUpdate>::read_from(source)?;
        let output_notes = BTreeMap::<NoteId, OutputNoteUpdate>::read_from(source)?;
        let nullifier_order = BTreeMap::<Nullifier, u32>::read_from(source)?;
        let input_notes_by_id = BTreeMap::<NoteId, NoteDetailsCommitment>::read_from(source)?;
        let input_notes_by_nullifier =
            BTreeMap::<Nullifier, NoteDetailsCommitment>::read_from(source)?;

        // Output notes always carry metadata, so this index can be safely derived from the records.
        let output_notes_by_nullifier = output_notes
            .iter()
            .filter_map(|(note_id, update)| {
                update.inner().nullifier().map(|nullifier| (nullifier, *note_id))
            })
            .collect();

        Ok(Self {
            input_notes,
            output_notes,
            input_notes_by_nullifier,
            input_notes_by_id,
            output_notes_by_nullifier,
            nullifier_order,
        })
    }
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

    use super::{NoteConsumption, NoteUpdateTracker};
    use crate::store::InputNoteRecord;
    use crate::store::input_note_states::{
        ConsumedExternalNoteState,
        ConsumedUnauthenticatedLocalNoteState,
        ExpectedNoteState,
        NoteSubmissionData,
        ProcessingUnauthenticatedNoteState,
    };
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

    /// A metadata-less expected note. It has no `NoteId` and is tracked by its details commitment.
    fn expected_note(seed: u64) -> InputNoteRecord {
        let state = ExpectedNoteState {
            metadata: None,
            after_block_num: BlockNumber::from(0u32),
            tag: None,
        };
        InputNoteRecord::new(note_details(seed), NoteAttachments::empty(), Some(0), state.into())
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

    /// A metadata-bearing note that is already consumed by a local transaction.
    fn consumed_local_note(seed: u64, sender: AccountId) -> InputNoteRecord {
        let state = ConsumedUnauthenticatedLocalNoteState {
            metadata: note_metadata(sender),
            nullifier_block_height: BlockNumber::from(1u32),
            submission_data: NoteSubmissionData {
                submitted_at: Some(0),
                consumer_account: sender,
                consumer_transaction: TransactionId::from_raw(Word::default()),
            },
            consumed_tx_order: Some(0),
        };
        InputNoteRecord::new(note_details(seed), NoteAttachments::empty(), Some(0), state.into())
    }

    /// A metadata-less note that is already externally consumed. It never carried a `NoteId`.
    fn consumed_external_note(seed: u64) -> InputNoteRecord {
        let state = ConsumedExternalNoteState {
            nullifier_block_height: BlockNumber::from(1u32),
            consumer_account: None,
            consumed_tx_order: None,
        };
        InputNoteRecord::new(note_details(seed), NoteAttachments::empty(), Some(0), state.into())
    }

    // TESTS
    // --------------------------------------------------------------------------------------------

    #[test]
    fn consumed_input_note_ids_reports_metadata_bearing_consumed_note() {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let note = consumed_local_note(1, sender);
        let id = note.id().expect("consumed-local note has metadata");

        let tracker = NoteUpdateTracker::for_transaction_updates(vec![], vec![note], vec![]);

        let consumed: alloc::vec::Vec<NoteId> = tracker.consumed_input_note_ids().collect();
        assert_eq!(consumed, vec![id]);
    }

    #[test]
    fn consumed_input_note_ids_omits_note_that_never_had_an_id() {
        // A note inserted already in the externally-consumed (metadata-less) state never had an id
        // in the tracker, so it is persisted but is not reported by id.
        let note = consumed_external_note(2);
        assert!(note.id().is_none());

        let tracker = NoteUpdateTracker::for_transaction_updates(vec![note], vec![], vec![]);

        assert_eq!(tracker.consumed_input_note_ids().count(), 0);
        assert_eq!(tracker.updated_input_notes().count(), 1);
    }

    #[test]
    fn external_consumption_retains_note_id() {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let note = processing_note(3, sender);
        let id = note.id().expect("processing note has metadata");
        let nullifier = note.nullifier().expect("processing note has metadata");

        let mut tracker = NoteUpdateTracker::for_transaction_updates(vec![], vec![note], vec![]);
        assert_eq!(tracker.consumed_input_note_ids().count(), 0);

        // External consumption transitions the note to a metadata-less state, dropping its id and
        // nullifier on the record. The id must still be reported via the retained index entry.
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

        // External consumption drops the record's id; the tracker retains it in
        // `input_notes_by_id` so the note can still be reported as consumed by its id.
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

    #[test]
    fn serialize_round_trip_preserves_lookup_indices() {
        let sender: AccountId = ACCOUNT_ID_SENDER.try_into().unwrap();
        let expected = expected_note(10);
        let processing = processing_note(11, sender);
        let processing_id = processing.id().expect("processing note has metadata");
        let processing_commitment = processing.details_commitment();
        let processing_nullifier = processing.nullifier().expect("processing note has metadata");

        let tracker =
            NoteUpdateTracker::for_transaction_updates(vec![expected], vec![processing], vec![]);

        let bytes = tracker.to_bytes();
        let restored = NoteUpdateTracker::read_from_bytes(&bytes).expect("round-trip should work");

        // The records and lookup indices round-trip unchanged, including the metadata-less note
        // that is keyed only by its details commitment.
        assert_eq!(tracker, restored);
        assert_eq!(restored.updated_input_notes().count(), 2);
        assert_eq!(
            restored.input_notes_by_id.get(&processing_id).copied(),
            Some(processing_commitment)
        );
        assert_eq!(
            restored.input_notes_by_nullifier.get(&processing_nullifier).copied(),
            Some(processing_commitment)
        );
    }
}

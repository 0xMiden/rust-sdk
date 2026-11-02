use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::mem;

use miden_protocol::note::{NoteDetailsCommitment, NoteId};
use miden_protocol::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use crate::store::{InputNoteRecord, OutputNoteRecord};

// NOTE UPDATE
// ================================================================================================

/// Represents the store write that a note in [`NoteUpdates`] needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NoteUpdateType {
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

impl TryFrom<u8> for NoteUpdateType {
    type Error = u8;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(NoteUpdateType::Insert),
            2 => Ok(NoteUpdateType::Update),
            3 => Ok(NoteUpdateType::InsertCommitted),
            other => Err(other),
        }
    }
}

/// An input note record and the store write it needs.
#[derive(Clone, Debug, PartialEq)]
pub struct InputNoteUpdate {
    /// Input note being updated.
    note: InputNoteRecord,
    /// Type of the note update.
    update_type: NoteUpdateType,
}

impl InputNoteUpdate {
    /// Creates a new [`InputNoteUpdate`].
    pub fn new(note: InputNoteRecord, update_type: NoteUpdateType) -> Self {
        Self { note, update_type }
    }

    /// Returns a reference the inner note record.
    pub fn inner(&self) -> &InputNoteRecord {
        &self.note
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

    /// Returns the per-account position of the consuming transaction within the account's execution
    /// chain for the block. `None` for non-consumed notes or when the order has not been determined
    /// yet.
    pub fn consumed_tx_order(&self) -> Option<u32> {
        self.note.state().consumed_tx_order()
    }
}

/// An output note record and the store write it needs.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputNoteUpdate {
    /// Output note being updated.
    note: OutputNoteRecord,
    /// Type of the note update.
    update_type: NoteUpdateType,
}

impl OutputNoteUpdate {
    /// Creates a new [`OutputNoteUpdate`].
    pub fn new(note: OutputNoteRecord, update_type: NoteUpdateType) -> Self {
        Self { note, update_type }
    }

    /// Returns a reference the inner note record.
    pub fn inner(&self) -> &OutputNoteRecord {
        &self.note
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

// NOTE UPDATES
// ================================================================================================

/// Contains the note changes to apply to the store.
///
/// Each entry is a new note to insert or an existing note to update.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NoteUpdates {
    /// New and updated input note records.
    input_notes: Vec<InputNoteUpdate>,
    /// New and updated output note records.
    output_notes: Vec<OutputNoteUpdate>,
}

impl NoteUpdates {
    /// Creates a new [`NoteUpdates`] from the provided input and output note updates.
    ///
    /// The store keys notes by details commitment, so updates with the same commitment become one
    /// update. A later update replaces an earlier one, with two exceptions: a record without
    /// metadata does not replace a record with metadata, and an `Update` keeps the write type of
    /// the earlier update.
    pub fn new(input_notes: Vec<InputNoteUpdate>, output_notes: Vec<OutputNoteUpdate>) -> Self {
        let mut inputs = BTreeMap::<NoteDetailsCommitment, InputNoteUpdate>::new();
        for mut update in input_notes {
            let commitment = update.inner().details_commitment();
            if let Some(existing) = inputs.get(&commitment) {
                if update.id().is_none() && existing.id().is_some() {
                    continue;
                }
                update.update_type = merged_update_type(existing.update_type, update.update_type);
            }
            inputs.insert(commitment, update);
        }

        let mut outputs = BTreeMap::<NoteDetailsCommitment, OutputNoteUpdate>::new();
        for mut update in output_notes {
            let commitment = update.inner().details_commitment();
            if let Some(existing) = outputs.get(&commitment) {
                update.update_type = merged_update_type(existing.update_type, update.update_type);
            }
            outputs.insert(commitment, update);
        }

        Self {
            input_notes: inputs.into_values().collect(),
            output_notes: outputs.into_values().collect(),
        }
    }

    /// Creates a [`NoteUpdates`] that inserts the new records and updates the updated records.
    pub fn from_records(
        new_input_notes: impl IntoIterator<Item = InputNoteRecord>,
        updated_input_notes: impl IntoIterator<Item = InputNoteRecord>,
        new_output_notes: impl IntoIterator<Item = OutputNoteRecord>,
    ) -> Self {
        let input_notes = new_input_notes
            .into_iter()
            .map(|note| InputNoteUpdate::new(note, NoteUpdateType::Insert))
            .chain(
                updated_input_notes
                    .into_iter()
                    .map(|note| InputNoteUpdate::new(note, NoteUpdateType::Update)),
            )
            .collect();

        let output_notes = new_output_notes
            .into_iter()
            .map(|note| OutputNoteUpdate::new(note, NoteUpdateType::Insert))
            .collect();

        Self::new(input_notes, output_notes)
    }

    /// Merges the note updates of a later transaction into these updates. The merge follows the
    /// rules of [`Self::new`], with the updates of `later` after the current updates.
    pub fn merge(&mut self, later: NoteUpdates) {
        let input_notes = mem::take(&mut self.input_notes).into_iter().chain(later.input_notes);
        let output_notes = mem::take(&mut self.output_notes).into_iter().chain(later.output_notes);
        *self = Self::new(input_notes.collect(), output_notes.collect());
    }

    /// Returns the new and updated input notes.
    ///
    /// Metadata-less expected notes (e.g. future notes created by a transaction, such as swap
    /// payback notes) are included as well: they have no `NoteId` yet but must still be persisted
    /// and have their tags registered.
    pub fn input_notes(&self) -> &[InputNoteUpdate] {
        &self.input_notes
    }

    /// Returns the new and updated output notes.
    pub fn output_notes(&self) -> &[OutputNoteUpdate] {
        &self.output_notes
    }

    /// Returns the ids of input notes that are now consumed. Notes without metadata have no id, so
    /// they are not included.
    pub fn consumed_input_note_ids(&self) -> impl Iterator<Item = NoteId> + '_ {
        self.input_notes
            .iter()
            .filter(|update| update.inner().is_consumed())
            .filter_map(InputNoteUpdate::id)
    }

    /// `NoteId`s of every input + output note that transitioned to a consumed state. These are
    /// confirmed consumptions, not raw nullifier-prefix hits.
    pub fn consumed_note_ids(&self) -> impl Iterator<Item = NoteId> + '_ {
        let output = self
            .output_notes
            .iter()
            .filter(|update| update.inner().is_consumed())
            .map(OutputNoteUpdate::id);
        self.consumed_input_note_ids().chain(output)
    }
}

/// Returns the write type for two updates of the same note. An `Update` keeps the earlier type, so
/// a note that needs an insert is not written as an update.
fn merged_update_type(earlier: NoteUpdateType, later: NoteUpdateType) -> NoteUpdateType {
    if later == NoteUpdateType::Update {
        earlier
    } else {
        later
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

impl Serializable for NoteUpdates {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.input_notes.write_into(target);
        self.output_notes.write_into(target);
    }
}

impl Deserializable for NoteUpdates {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let input_notes = Vec::<InputNoteUpdate>::read_from(source)?;
        let output_notes = Vec::<OutputNoteUpdate>::read_from(source)?;
        Ok(Self { input_notes, output_notes })
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
    use miden_protocol::{Felt, Word, ZERO};
    use miden_standards::note::StandardNote;

    use super::NoteUpdates;
    use crate::store::InputNoteRecord;
    use crate::store::input_note_states::{
        ConsumedExternalNoteState,
        ConsumedUnauthenticatedLocalNoteState,
        NoteSubmissionData,
    };

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
            metadata: None,
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

        let note_updates = NoteUpdates::from_records(vec![], vec![note], vec![]);

        let consumed: alloc::vec::Vec<NoteId> = note_updates.consumed_input_note_ids().collect();
        assert_eq!(consumed, vec![id]);
    }

    #[test]
    fn consumed_input_note_ids_omits_note_without_id() {
        // A metadata-less note has no id, so it is persisted but is not reported by id.
        let note = consumed_external_note(2);
        assert!(note.id().is_none());

        let note_updates = NoteUpdates::from_records(vec![note], vec![], vec![]);

        assert_eq!(note_updates.consumed_input_note_ids().count(), 0);
        assert_eq!(note_updates.input_notes().len(), 1);
    }
}

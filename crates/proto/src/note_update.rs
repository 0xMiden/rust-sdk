//! Protobuf conversion of note update values.

use std::collections::BTreeMap;
use std::string::ToString;

use miden_client::Word;
use miden_client::note::{InputNoteUpdate, NoteUpdateTracker, NoteUpdateType, OutputNoteUpdate};
use miden_client::store::{InputNoteRecord, OutputNoteRecord};
use miden_client::utils::Serializable;
use miden_protocol::note::{NoteDetailsCommitment, NoteId, Nullifier};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, read_upstream, required};

impl ProtobufValue for InputNoteUpdate {
    type Message = proto::InputNoteUpdate;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            note: Some(self.inner().to_proto()),
            update_type: encode_update_type(*self.update_type()).into(),
        }
    }

    fn from_proto(update: Self::Message) -> Result<Self, ProtoDecodeError> {
        let note = required(update.note, "input note update", "note")?;
        Ok(InputNoteUpdate::from_parts(
            InputNoteRecord::from_proto(note)?,
            decode_update_type(update.update_type)?,
        ))
    }
}

impl ProtobufValue for OutputNoteUpdate {
    type Message = proto::OutputNoteUpdate;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            note: Some(self.inner().to_proto()),
            update_type: encode_update_type(*self.update_type()).into(),
        }
    }

    fn from_proto(update: Self::Message) -> Result<Self, ProtoDecodeError> {
        let note = required(update.note, "output note update", "note")?;
        Ok(OutputNoteUpdate::from_parts(
            OutputNoteRecord::from_proto(note)?,
            decode_update_type(update.update_type)?,
        ))
    }
}

impl ProtobufValue for NoteUpdateTracker {
    type Message = proto::NoteUpdateTracker;

    fn to_proto(&self) -> Self::Message {
        use proto::note_update_tracker::{NoteIdEntry, NullifierEntry, NullifierOrder};

        Self::Message {
            input_notes: self.input_note_updates().map(ProtobufValue::to_proto).collect(),
            output_notes: self.output_note_updates().map(ProtobufValue::to_proto).collect(),
            nullifier_order: self
                .nullifier_order()
                .iter()
                .map(|(nullifier, order)| NullifierOrder {
                    nullifier: nullifier.to_bytes(),
                    order: *order,
                })
                .collect(),
            input_notes_by_id: self
                .input_notes_by_id()
                .iter()
                .map(|(note_id, commitment)| NoteIdEntry {
                    note_id: Some(note_id.as_word().into()),
                    details_commitment: commitment.to_bytes(),
                })
                .collect(),
            input_notes_by_nullifier: self
                .input_notes_by_nullifier()
                .iter()
                .map(|(nullifier, commitment)| NullifierEntry {
                    nullifier: nullifier.to_bytes(),
                    details_commitment: commitment.to_bytes(),
                })
                .collect(),
        }
    }

    fn from_proto(tracker: Self::Message) -> Result<Self, ProtoDecodeError> {
        let input_notes = tracker
            .input_notes
            .into_iter()
            .map(InputNoteUpdate::from_proto)
            .collect::<Result<Vec<_>, _>>()?;
        let output_notes = tracker
            .output_notes
            .into_iter()
            .map(OutputNoteUpdate::from_proto)
            .collect::<Result<Vec<_>, _>>()?;
        let nullifier_order = tracker
            .nullifier_order
            .into_iter()
            .map(|entry| Ok((read_upstream::<Nullifier>(&entry.nullifier)?, entry.order)))
            .collect::<Result<BTreeMap<_, _>, ProtoDecodeError>>()?;
        let input_notes_by_id = tracker
            .input_notes_by_id
            .into_iter()
            .map(|entry| {
                let note_id: Word =
                    required(entry.note_id, "note id entry", "note id")?.try_into()?;
                Ok((
                    NoteId::from_raw(note_id),
                    read_upstream::<NoteDetailsCommitment>(&entry.details_commitment)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ProtoDecodeError>>()?;
        let input_notes_by_nullifier = tracker
            .input_notes_by_nullifier
            .into_iter()
            .map(|entry| {
                Ok((
                    read_upstream::<Nullifier>(&entry.nullifier)?,
                    read_upstream::<NoteDetailsCommitment>(&entry.details_commitment)?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ProtoDecodeError>>()?;

        Ok(NoteUpdateTracker::from_parts(
            input_notes,
            output_notes,
            nullifier_order,
            input_notes_by_id,
            input_notes_by_nullifier,
        ))
    }
}

fn encode_update_type(update_type: NoteUpdateType) -> proto::NoteUpdateType {
    match update_type {
        NoteUpdateType::None => proto::NoteUpdateType::None,
        NoteUpdateType::Insert => proto::NoteUpdateType::Insert,
        NoteUpdateType::Update => proto::NoteUpdateType::Update,
        NoteUpdateType::InsertCommitted => proto::NoteUpdateType::InsertCommitted,
    }
}

fn decode_update_type(value: i32) -> Result<NoteUpdateType, ProtoDecodeError> {
    match proto::NoteUpdateType::try_from(value)? {
        proto::NoteUpdateType::Unspecified => {
            Err(ProtoDecodeError::InvalidValue("note update has no update type".to_string()))
        },
        proto::NoteUpdateType::None => Ok(NoteUpdateType::None),
        proto::NoteUpdateType::Insert => Ok(NoteUpdateType::Insert),
        proto::NoteUpdateType::Update => Ok(NoteUpdateType::Update),
        proto::NoteUpdateType::InsertCommitted => Ok(NoteUpdateType::InsertCommitted),
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use miden_client::note::{
        InputNoteUpdate,
        NoteUpdateTracker,
        NoteUpdateType,
        OutputNoteUpdate,
    };
    use miden_client::store::{InputNoteRecord, OutputNoteRecord};
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{Note, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_standards::note::P2idNote;

    use crate::{decode, encode};

    fn note(serial: u32) -> Note {
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        P2idNote::builder()
            .sender(AccountId::try_from(ACCOUNT_ID_SENDER).unwrap())
            .target(AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap())
            .assets(vec![FungibleAsset::new(faucet_id, 100).unwrap()])
            .note_type(NoteType::Private)
            .serial_number(Word::from([serial, 0, 0, 0]))
            .build()
            .unwrap()
            .into()
    }

    /// The indices go through as they are. A tracker can hold an index entry that its records do
    /// not imply, such as the id of a note that was consumed externally.
    #[test]
    fn note_update_tracker_round_trips_with_its_indices() {
        let input = note(1);
        let output = note(2);
        let retained = note(3);
        let input_record = InputNoteRecord::from(input.clone());

        let tracker = NoteUpdateTracker::from_parts(
            [InputNoteUpdate::from_parts(input_record.clone(), NoteUpdateType::Insert)],
            [OutputNoteUpdate::from_parts(
                OutputNoteRecord::from_full_note(output, BlockNumber::from(5u32)),
                NoteUpdateType::Update,
            )],
            BTreeMap::from([(input.nullifier(), 7)]),
            BTreeMap::from([
                (input.id(), input_record.details_commitment()),
                (retained.id(), input_record.details_commitment()),
            ]),
            BTreeMap::from([(input.nullifier(), input_record.details_commitment())]),
        );

        assert_eq!(decode::<NoteUpdateTracker>(&encode(&tracker)).unwrap(), tracker);
    }
}

//! Protobuf conversion of note update values.

use std::string::ToString;

use miden_client_core::store::{
    InputNoteRecord,
    InputNoteUpdate,
    NoteUpdateType,
    NoteUpdates,
    OutputNoteRecord,
    OutputNoteUpdate,
};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

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
        Ok(InputNoteUpdate::new(
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
        Ok(OutputNoteUpdate::new(
            OutputNoteRecord::from_proto(note)?,
            decode_update_type(update.update_type)?,
        ))
    }
}

impl ProtobufValue for NoteUpdates {
    type Message = proto::NoteUpdates;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            input_notes: self.input_notes().iter().map(ProtobufValue::to_proto).collect(),
            output_notes: self.output_notes().iter().map(ProtobufValue::to_proto).collect(),
        }
    }

    fn from_proto(updates: Self::Message) -> Result<Self, ProtoDecodeError> {
        let input_notes = updates
            .input_notes
            .into_iter()
            .map(InputNoteUpdate::from_proto)
            .collect::<Result<Vec<_>, _>>()?;
        let output_notes = updates
            .output_notes
            .into_iter()
            .map(OutputNoteUpdate::from_proto)
            .collect::<Result<Vec<_>, _>>()?;

        Ok(NoteUpdates::new(input_notes, output_notes))
    }
}

fn encode_update_type(update_type: NoteUpdateType) -> proto::NoteUpdateType {
    match update_type {
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
        proto::NoteUpdateType::Insert => Ok(NoteUpdateType::Insert),
        proto::NoteUpdateType::Update => Ok(NoteUpdateType::Update),
        proto::NoteUpdateType::InsertCommitted => Ok(NoteUpdateType::InsertCommitted),
    }
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_client_core::store::{
        InputNoteRecord,
        InputNoteUpdate,
        NoteUpdateType,
        NoteUpdates,
        OutputNoteRecord,
        OutputNoteUpdate,
    };
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

    use crate as proto;
    use crate::{ProtoDecodeError, ProtobufValue, decode, encode};

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

    #[test]
    fn note_updates_round_trip_with_every_update_type() {
        let updates = NoteUpdates::new(
            vec![
                InputNoteUpdate::new(InputNoteRecord::from(note(1)), NoteUpdateType::Insert),
                InputNoteUpdate::new(InputNoteRecord::from(note(2)), NoteUpdateType::Update),
            ],
            vec![OutputNoteUpdate::new(
                OutputNoteRecord::from_full_note(note(3), BlockNumber::from(5u32)),
                NoteUpdateType::InsertCommitted,
            )],
        );

        assert_eq!(decode::<NoteUpdates>(&encode(&updates)).unwrap(), updates);
    }

    /// An update needs its note record. A missing record is an error, not a default value.
    #[test]
    fn note_updates_reject_an_update_without_its_note() {
        let message = proto::NoteUpdates {
            input_notes: vec![proto::InputNoteUpdate {
                note: None,
                update_type: proto::NoteUpdateType::Insert.into(),
            }],
            ..Default::default()
        };

        assert!(matches!(
            NoteUpdates::from_proto(message),
            Err(ProtoDecodeError::MissingField { field: "note", .. })
        ));
    }

    /// The update type selects the store write, so an absent type is rejected.
    #[test]
    fn note_updates_reject_an_unspecified_update_type() {
        let record = OutputNoteRecord::from_full_note(note(1), BlockNumber::from(1u32));
        let message = proto::NoteUpdates {
            output_notes: vec![proto::OutputNoteUpdate {
                note: Some(record.to_proto()),
                update_type: proto::NoteUpdateType::Unspecified.into(),
            }],
            ..Default::default()
        };

        assert!(matches!(
            NoteUpdates::from_proto(message),
            Err(ProtoDecodeError::InvalidValue(_))
        ));
    }
}

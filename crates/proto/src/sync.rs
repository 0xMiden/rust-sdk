//! Protobuf conversion of sync values.

use miden_client_core::sync::{NoteTagRecord, NoteTagSource};
use miden_objects::DecodeMessageExt;
use miden_protocol::note::{NoteDetailsCommitment, NoteTag};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for NoteTagSource {
    type Message = proto::NoteTagSource;

    fn to_proto(&self) -> Self::Message {
        use proto::note_tag_source::{Source, User};

        let source = match self {
            NoteTagSource::Account(account_id) => Source::Account((*account_id).into()),
            NoteTagSource::Note(commitment) => Source::Note(commitment.as_word().into()),
            NoteTagSource::User => Source::User(User {}),
            NoteTagSource::Subscription(key) => Source::Subscription((*key).into()),
        };
        Self::Message { source: Some(source) }
    }

    fn from_proto(source: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::note_tag_source::Source;

        Ok(match required(source.source, "note tag source", "source")? {
            Source::Account(account_id) => NoteTagSource::Account(account_id.decode_and_verify()?),
            Source::Note(commitment) => {
                NoteTagSource::Note(NoteDetailsCommitment::from_raw(commitment.try_into()?))
            },
            Source::User(_) => NoteTagSource::User,
            Source::Subscription(key) => NoteTagSource::Subscription(key.try_into()?),
        })
    }
}

impl ProtobufValue for NoteTagRecord {
    type Message = proto::NoteTagRecord;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            tag: self.tag.as_u32(),
            source: Some(self.source.to_proto()),
        }
    }

    fn from_proto(record: Self::Message) -> Result<Self, ProtoDecodeError> {
        let source = required(record.source, "note tag record", "source")?;
        Ok(NoteTagRecord {
            tag: NoteTag::from(record.tag),
            source: NoteTagSource::from_proto(source)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use miden_client_core::sync::{NoteTagRecord, NoteTagSource};
    use miden_objects::proto::primitives;
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::note::{NoteDetailsCommitment, NoteTag};
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;

    use crate as proto;
    use crate::{ProtoDecodeError, ProtobufValue, decode, encode};

    #[test]
    fn note_tag_record_round_trips_with_every_source() {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let word = Word::from([1u32, 2, 3, 4]);

        for source in [
            NoteTagSource::Account(account_id),
            NoteTagSource::Note(NoteDetailsCommitment::from_raw(word)),
            NoteTagSource::User,
            NoteTagSource::Subscription(word),
        ] {
            let record = NoteTagRecord { tag: NoteTag::new(0x0102_0304), source };
            assert_eq!(decode::<NoteTagRecord>(&encode(&record)).unwrap(), record);
        }
    }

    #[test]
    fn note_tag_record_rejects_a_malformed_note_commitment() {
        let message = proto::NoteTagRecord {
            tag: 0,
            source: Some(proto::NoteTagSource {
                source: Some(proto::note_tag_source::Source::Note(primitives::Word {
                    encoded: vec![0; 31],
                })),
            }),
        };

        assert!(matches!(
            NoteTagRecord::from_proto(message),
            Err(ProtoDecodeError::Conversion(_))
        ));
    }
}

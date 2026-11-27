//! Protobuf conversion of sync values.

use miden_client_core::sync::NoteTagSource;
use miden_objects::DecodeMessageExt;
use miden_protocol::note::NoteDetailsCommitment;

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

#[cfg(test)]
mod tests {
    use miden_client_core::sync::NoteTagSource;
    use miden_objects::proto::primitives;
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::note::NoteDetailsCommitment;
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE;

    use crate as proto;
    use crate::{ProtoDecodeError, decode, encode};

    #[test]
    fn note_tag_source_round_trips_with_every_variant() {
        let account_id =
            AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap();
        let word = Word::from([1u32, 2, 3, 4]);

        for source in [
            NoteTagSource::Account(account_id),
            NoteTagSource::Note(NoteDetailsCommitment::from_raw(word)),
            NoteTagSource::User,
            NoteTagSource::Subscription(word),
        ] {
            assert_eq!(decode::<NoteTagSource>(&encode(&source)).unwrap(), source);
        }
    }

    #[test]
    fn note_tag_source_rejects_a_malformed_note_commitment() {
        let message = proto::NoteTagSource {
            source: Some(proto::note_tag_source::Source::Note(primitives::Word {
                encoded: vec![0; 31],
            })),
        };

        assert!(matches!(
            decode::<NoteTagSource>(&prost::Message::encode_to_vec(&message)),
            Err(ProtoDecodeError::Conversion(_))
        ));
    }
}

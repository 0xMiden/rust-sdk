//! Protobuf conversion of sync values.

#[cfg(feature = "client")]
use miden_client::sync::SyncSummary;
use miden_client_core::sync::{NoteTagRecord, NoteTagSource};
use miden_objects::DecodeMessageExt;
#[cfg(feature = "client")]
use miden_protocol::note::NoteId;
use miden_protocol::note::{NoteDetailsCommitment, NoteTag};

use crate as proto;
#[cfg(feature = "client")]
use crate::decode_all;
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

#[cfg(feature = "client")]
impl ProtobufValue for SyncSummary {
    type Message = proto::SyncSummary;

    fn to_proto(&self) -> Self::Message {
        let note_ids = |ids: &[NoteId]| ids.iter().map(Into::into).collect();

        Self::Message {
            block_num: Some(self.block_num.into()),
            new_public_notes: note_ids(&self.new_public_notes),
            new_private_notes: note_ids(&self.new_private_notes),
            committed_notes: note_ids(&self.committed_notes),
            consumed_notes: note_ids(&self.consumed_notes),
            updated_accounts: self.updated_accounts.iter().map(|id| (*id).into()).collect(),
            locked_accounts: self.locked_accounts.iter().map(|id| (*id).into()).collect(),
            committed_transactions: self.committed_transactions.iter().map(Into::into).collect(),
        }
    }

    fn from_proto(summary: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(SyncSummary::new(
            required(summary.block_num, "sync summary", "block number")?.decode_and_verify()?,
            decode_all(summary.new_public_notes)?,
            decode_all(summary.new_private_notes)?,
            decode_all(summary.committed_notes)?,
            decode_all(summary.consumed_notes)?,
            decode_all(summary.updated_accounts)?,
            decode_all(summary.locked_accounts)?,
            decode_all(summary.committed_transactions)?,
        ))
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

    #[cfg(feature = "client")]
    fn sync_summary() -> miden_client::sync::SyncSummary {
        use miden_protocol::block::BlockNumber;
        use miden_protocol::note::NoteId;
        use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;
        use miden_protocol::transaction::TransactionId;

        let note_id = |seed: u32| NoteId::from_raw(Word::from([seed, 0, 0, 0]));
        miden_client::sync::SyncSummary::new(
            BlockNumber::from(9u32),
            vec![note_id(1)],
            vec![note_id(2)],
            vec![note_id(3)],
            vec![note_id(4)],
            vec![AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap()],
            vec![AccountId::try_from(ACCOUNT_ID_SENDER).unwrap()],
            vec![TransactionId::from_raw(Word::from([5u32, 0, 0, 0]))],
        )
    }

    /// Each list holds a different ID, so a field mix-up causes a mismatch.
    #[cfg(feature = "client")]
    #[test]
    fn sync_summary_round_trips() {
        let summary = sync_summary();
        assert_eq!(decode::<miden_client::sync::SyncSummary>(&encode(&summary)).unwrap(), summary);
    }

    #[cfg(feature = "client")]
    #[test]
    fn sync_summary_rejects_a_malformed_note_id() {
        let mut message = sync_summary().to_proto();
        message.committed_notes[0] = miden_objects::proto::note::NoteId {
            id: Some(primitives::Word { encoded: vec![0; 31] }),
        };

        assert!(matches!(
            miden_client::sync::SyncSummary::from_proto(message),
            Err(ProtoDecodeError::Conversion(_))
        ));
    }
}

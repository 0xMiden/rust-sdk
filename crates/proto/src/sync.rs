//! Protobuf conversion of sync values.

use std::vec::Vec;

use miden_client::Word;
use miden_client::sync::{NoteTagRecord, NoteTagSource, SyncSummary};
use miden_client::utils::Serializable;
use miden_objects::{DecodeMessageExt, proto as objects};
use miden_protocol::note::{NoteDetailsCommitment, NoteId, NoteTag};
use miden_protocol::transaction::TransactionId;

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, read_upstream, required};

impl ProtobufValue for NoteTagRecord {
    type Message = proto::NoteTagRecord;

    fn to_proto(&self) -> Self::Message {
        use proto::note_tag_record::{Source, User};

        let source = match &self.source {
            NoteTagSource::Account(account_id) => Source::Account((*account_id).into()),
            NoteTagSource::Note(commitment) => Source::Note(commitment.to_bytes()),
            NoteTagSource::User => Source::User(User {}),
            NoteTagSource::Subscription(key) => Source::Subscription((*key).into()),
        };
        Self::Message {
            tag: self.tag.as_u32(),
            source: Some(source),
        }
    }

    fn from_proto(record: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::note_tag_record::Source;

        let source = match required(record.source, "note tag record", "source")? {
            Source::Account(account_id) => NoteTagSource::Account(account_id.decode_and_verify()?),
            Source::Note(commitment) => {
                NoteTagSource::Note(read_upstream::<NoteDetailsCommitment>(&commitment)?)
            },
            Source::User(_) => NoteTagSource::User,
            Source::Subscription(key) => NoteTagSource::Subscription(key.try_into()?),
        };
        Ok(NoteTagRecord { tag: NoteTag::from(record.tag), source })
    }
}

impl ProtobufValue for SyncSummary {
    type Message = proto::SyncSummary;

    fn to_proto(&self) -> Self::Message {
        let note_ids = |ids: &[NoteId]| ids.iter().map(|id| id.as_word().into()).collect();

        Self::Message {
            block_num: Some(self.block_num.into()),
            new_public_notes: note_ids(&self.new_public_notes),
            new_private_notes: note_ids(&self.new_private_notes),
            committed_notes: note_ids(&self.committed_notes),
            consumed_notes: note_ids(&self.consumed_notes),
            updated_accounts: self.updated_accounts.iter().map(|id| (*id).into()).collect(),
            locked_accounts: self.locked_accounts.iter().map(|id| (*id).into()).collect(),
            committed_transactions: self
                .committed_transactions
                .iter()
                .map(|id| id.as_word().into())
                .collect(),
        }
    }

    fn from_proto(summary: Self::Message) -> Result<Self, ProtoDecodeError> {
        fn note_ids(
            words: Vec<objects::primitives::Word>,
        ) -> Result<Vec<NoteId>, ProtoDecodeError> {
            words
                .into_iter()
                .map(|word| Ok(NoteId::from_raw(Word::try_from(word)?)))
                .collect()
        }
        fn account_ids(
            ids: Vec<objects::account::AccountId>,
        ) -> Result<Vec<miden_client::account::AccountId>, ProtoDecodeError> {
            ids.into_iter().map(|id| Ok(id.decode_and_verify()?)).collect()
        }

        let committed_transactions = summary
            .committed_transactions
            .into_iter()
            .map(|word| Ok(TransactionId::from_raw(Word::try_from(word)?)))
            .collect::<Result<Vec<_>, ProtoDecodeError>>()?;

        Ok(SyncSummary::new(
            required(summary.block_num, "sync summary", "block number")?.decode_and_verify()?,
            note_ids(summary.new_public_notes)?,
            note_ids(summary.new_private_notes)?,
            note_ids(summary.committed_notes)?,
            note_ids(summary.consumed_notes)?,
            account_ids(summary.updated_accounts)?,
            account_ids(summary.locked_accounts)?,
            committed_transactions,
        ))
    }
}

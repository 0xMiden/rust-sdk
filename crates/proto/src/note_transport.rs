//! Protobuf conversion of note transport values.

use std::collections::BTreeMap;
#[cfg(feature = "testing")]
use std::vec::Vec;

#[cfg(feature = "client")]
use miden_client::note_transport::NoteInfo;
#[cfg(feature = "testing")]
use miden_client::testing::note_transport::MockNoteTransportNode;
use miden_client_core::note_transport::NoteTransportCursor;
#[cfg(feature = "client")]
use miden_objects::DecodeMessageExt;
use miden_protocol::note::NoteTag;

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for NoteTransportCursor {
    type Message = proto::NoteTransportCursor;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            position: self.parts().map(|(nonce, sequence)| {
                proto::note_transport_cursor::Position { nonce, sequence }
            }),
        }
    }

    fn from_proto(cursor: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(match cursor.position {
            Some(position) => NoteTransportCursor::from_parts(position.nonce, position.sequence),
            None => NoteTransportCursor::init(),
        })
    }
}

impl ProtobufValue for BTreeMap<NoteTag, NoteTransportCursor> {
    type Message = proto::NoteTransportCursors;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            entries: self
                .iter()
                .map(|(tag, cursor)| proto::note_transport_cursors::Entry {
                    tag: tag.as_u32(),
                    cursor: Some(cursor.to_proto()),
                })
                .collect(),
        }
    }

    fn from_proto(cursors: Self::Message) -> Result<Self, ProtoDecodeError> {
        cursors
            .entries
            .into_iter()
            .map(|entry| {
                let cursor = required(entry.cursor, "note transport cursors entry", "cursor")?;
                Ok((NoteTag::from(entry.tag), NoteTransportCursor::from_proto(cursor)?))
            })
            .collect()
    }
}

#[cfg(feature = "client")]
impl ProtobufValue for NoteInfo {
    type Message = proto::NoteInfo;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            header: Some(self.header.into()),
            details: self.details_bytes.clone(),
            block_hint: self.block_hint.map(Into::into),
        }
    }

    fn from_proto(info: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(NoteInfo {
            header: required(info.header, "note info", "header")?.decode_and_verify()?,
            details_bytes: info.details,
            block_hint: info.block_hint.map(DecodeMessageExt::decode_and_verify).transpose()?,
        })
    }
}

#[cfg(feature = "testing")]
impl ProtobufValue for MockNoteTransportNode {
    type Message = proto::MockNoteTransportNode;

    fn to_proto(&self) -> Self::Message {
        use proto::mock_note_transport_node::{Entry, TagNotes};

        Self::Message {
            notes: self
                .notes()
                .iter()
                .map(|(tag, notes)| TagNotes {
                    tag: tag.as_u32(),
                    notes: notes
                        .iter()
                        .map(|(note, cursor)| Entry {
                            note: Some(note.to_proto()),
                            cursor: Some(cursor.to_proto()),
                        })
                        .collect(),
                })
                .collect(),
            nonce: self.nonce(),
            next_sequence: self.next_sequence(),
        }
    }

    fn from_proto(node: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "mock note transport entry";

        let notes = node
            .notes
            .into_iter()
            .map(|tag_notes| {
                let notes = tag_notes
                    .notes
                    .into_iter()
                    .map(|entry| {
                        Ok((
                            NoteInfo::from_proto(required(entry.note, MESSAGE, "note")?)?,
                            NoteTransportCursor::from_proto(required(
                                entry.cursor,
                                MESSAGE,
                                "cursor",
                            )?)?,
                        ))
                    })
                    .collect::<Result<Vec<_>, ProtoDecodeError>>()?;
                Ok((NoteTag::from(tag_notes.tag), notes))
            })
            .collect::<Result<BTreeMap<_, _>, ProtoDecodeError>>()?;

        Ok(MockNoteTransportNode::from_parts(notes, node.nonce, node.next_sequence))
    }
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use miden_client::note_transport::NoteInfo;
    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::asset::FungibleAsset;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::note::{Note, NoteHeader, NoteType};
    use miden_protocol::testing::account_id::{
        ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
        ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE,
        ACCOUNT_ID_SENDER,
    };
    use miden_standards::note::P2idNote;

    use crate::{ProtoDecodeError, ProtobufValue, decode, encode};

    fn header() -> NoteHeader {
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let note: Note = P2idNote::builder()
            .sender(AccountId::try_from(ACCOUNT_ID_SENDER).unwrap())
            .target(AccountId::try_from(ACCOUNT_ID_REGULAR_PUBLIC_ACCOUNT_IMMUTABLE_CODE).unwrap())
            .assets(vec![FungibleAsset::new(faucet_id, 100).unwrap()])
            .note_type(NoteType::Private)
            .serial_number(Word::from([1u32, 0, 0, 0]))
            .build()
            .unwrap()
            .into();
        *note.header()
    }

    /// The details stay in the bytes that the service sent.
    #[test]
    fn note_info_round_trips() {
        let info = NoteInfo {
            header: header(),
            details_bytes: vec![1, 2, 3],
            block_hint: Some(BlockNumber::from(7u32)),
        };

        let decoded = decode::<NoteInfo>(&encode(&info)).unwrap();

        assert_eq!(decoded.header, info.header);
        assert_eq!(decoded.details_bytes, info.details_bytes);
        assert_eq!(decoded.block_hint, info.block_hint);
    }

    #[test]
    fn note_info_rejects_a_missing_header() {
        let info = NoteInfo {
            header: header(),
            details_bytes: vec![],
            block_hint: None,
        };
        let mut message = info.to_proto();
        message.header = None;

        assert!(matches!(
            NoteInfo::from_proto(message),
            Err(ProtoDecodeError::MissingField { field: "header", .. })
        ));
    }
}

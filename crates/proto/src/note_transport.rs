//! Protobuf conversion of note transport values.

use std::collections::BTreeMap;
#[cfg(feature = "testing")]
use std::vec::Vec;

use miden_client::note_transport::{NoteInfo, NoteTransportCursor};
#[cfg(feature = "testing")]
use miden_client::testing::note_transport::MockNoteTransportNode;
use miden_client::utils::Serializable;
use miden_objects::DecodeMessageExt;
use miden_protocol::note::NoteTag;

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, read_upstream, required};

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

impl ProtobufValue for NoteInfo {
    type Message = proto::NoteInfo;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            header: self.header.to_bytes(),
            details: self.details_bytes.clone(),
            block_hint: self.block_hint.map(Into::into),
        }
    }

    fn from_proto(info: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(NoteInfo {
            header: read_upstream(&info.header)?,
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

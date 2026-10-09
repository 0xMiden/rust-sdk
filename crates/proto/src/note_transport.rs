//! Protobuf conversion of note transport values.

use std::collections::BTreeMap;

use miden_client::note_transport::NoteTransportCursor;
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

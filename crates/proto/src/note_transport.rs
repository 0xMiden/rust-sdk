//! Protobuf conversion of note transport values.

use std::collections::BTreeMap;

use miden_client_core::note_transport::{NoteTransportCursor, NoteTransportSyncState};
use miden_protocol::note::NoteTag;

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for Option<NoteTransportCursor> {
    type Message = proto::NoteTransportCursor;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            position: self.map(|NoteTransportCursor { nonce, sequence }| {
                proto::note_transport_cursor::Position { nonce, sequence }
            }),
        }
    }

    fn from_proto(cursor: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(cursor.position.map(|position| NoteTransportCursor {
            nonce: position.nonce,
            sequence: position.sequence,
        }))
    }
}

impl ProtobufValue for BTreeMap<NoteTag, Option<NoteTransportCursor>> {
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
                Ok((NoteTag::from(entry.tag), Option::<NoteTransportCursor>::from_proto(cursor)?))
            })
            .collect()
    }
}

impl ProtobufValue for NoteTransportSyncState {
    type Message = proto::NoteTransportSyncState;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            cursor: Some(Some(self.cursor).to_proto()),
            covered_tags: self.covered_tags.iter().map(NoteTag::as_u32).collect(),
        }
    }

    fn from_proto(state: Self::Message) -> Result<Self, ProtoDecodeError> {
        let cursor = required(state.cursor, "note transport sync state", "cursor")?;
        let cursor = Option::<NoteTransportCursor>::from_proto(cursor)?;
        Ok(Self {
            cursor: required(cursor, "note transport sync state", "cursor position")?,
            covered_tags: state.covered_tags.into_iter().map(NoteTag::from).collect(),
        })
    }
}

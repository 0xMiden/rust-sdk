//! Protobuf conversion of note transport values.

use miden_client_core::note_transport::{NoteTransportCursor, NoteTransportSyncState};
use miden_protocol::note::NoteTag;

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for NoteTransportCursor {
    type Message = proto::NoteTransportCursor;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            nonce: self.nonce,
            sequence: self.sequence,
        }
    }

    fn from_proto(cursor: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(Self {
            nonce: cursor.nonce,
            sequence: cursor.sequence,
        })
    }
}

impl ProtobufValue for NoteTransportSyncState {
    type Message = proto::NoteTransportSyncState;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            cursor: Some(self.cursor.to_proto()),
            covered_tags: self.covered_tags.iter().map(NoteTag::as_u32).collect(),
        }
    }

    fn from_proto(state: Self::Message) -> Result<Self, ProtoDecodeError> {
        let cursor = required(state.cursor, "note transport sync state", "cursor")?;
        Ok(Self {
            cursor: NoteTransportCursor::from_proto(cursor)?,
            covered_tags: state.covered_tags.into_iter().map(NoteTag::from).collect(),
        })
    }
}

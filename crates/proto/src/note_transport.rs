//! Protobuf conversion of note transport values.

use std::collections::BTreeSet;

use miden_client::note_transport::NoteTransportCursor;
use miden_protocol::note::NoteTag;

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue};

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

impl ProtobufValue for BTreeSet<NoteTag> {
    type Message = proto::NoteTags;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            tags: self.iter().map(NoteTag::as_u32).collect(),
        }
    }

    fn from_proto(tags: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(tags.tags.into_iter().map(NoteTag::from).collect())
    }
}

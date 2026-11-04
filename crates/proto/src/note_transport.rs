//! Protobuf conversion of note transport values.

use miden_client_core::note_transport::{NoteInfo, NoteTransportCursor, NoteTransportSyncState};
use miden_objects::DecodeMessageExt;
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

#[cfg(test)]
mod tests {
    use miden_client_core::note_transport::NoteInfo;
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

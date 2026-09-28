use std::collections::BTreeSet;
use std::string::ToString;
use std::vec::Vec;

use miden_client::note::NoteTag;
use miden_client::note_transport::NoteInfo;
use miden_client::pswap::{PswapLineageRecord, PswapLineageState, build_record_from_fields};
use miden_client::rpc::RpcLimits;
use miden_objects::DecodeMessageExt;
use miden_protocol::asset::AssetAmount;

use crate::proto::{self, ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for RpcLimits {
    type Message = proto::RpcLimits;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            note_ids_limit: self.note_ids_limit,
            nullifiers_limit: self.nullifiers_limit,
            account_ids_limit: self.account_ids_limit,
            note_tags_limit: self.note_tags_limit,
        }
    }

    fn from_proto(limits: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(Self {
            note_ids_limit: limits.note_ids_limit,
            nullifiers_limit: limits.nullifiers_limit,
            account_ids_limit: limits.account_ids_limit,
            note_tags_limit: limits.note_tags_limit,
        })
    }
}

/// The notes that wait to be relayed to the note transport network.
impl ProtobufValue for Vec<NoteInfo> {
    type Message = proto::RelayOutbox;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            notes: self.iter().map(Into::into).collect(),
        }
    }

    fn from_proto(outbox: Self::Message) -> Result<Self, ProtoDecodeError> {
        outbox.notes.into_iter().map(NoteInfo::try_from).collect()
    }
}

/// The note tags whose history the client already fetched from the note transport network.
impl ProtobufValue for BTreeSet<NoteTag> {
    type Message = proto::NoteTags;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            tags: self.iter().copied().map(u32::from).collect(),
        }
    }

    fn from_proto(tags: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(tags.tags.into_iter().map(NoteTag::from).collect())
    }
}

impl From<&NoteInfo> for proto::NoteInfo {
    fn from(info: &NoteInfo) -> Self {
        Self {
            header: Some(info.header.into()),
            details: info.details_bytes.clone(),
            block_hint: info.block_hint.map(Into::into),
        }
    }
}

impl TryFrom<proto::NoteInfo> for NoteInfo {
    type Error = ProtoDecodeError;

    fn try_from(info: proto::NoteInfo) -> Result<Self, Self::Error> {
        Ok(NoteInfo {
            header: required(info.header, "note info", "header")?.decode_and_verify()?,
            details_bytes: info.details,
            block_hint: info.block_hint.map(DecodeMessageExt::decode_and_verify).transpose()?,
        })
    }
}

/// Only the remaining *amounts* are stored. The faucets and the full note live on the depth-0 note,
/// which is recovered through `original_note_id` when needed.
impl ProtobufValue for PswapLineageRecord {
    type Message = proto::PswapLineageRecord;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            original_note_id: Some((&self.original_note_id).into()),
            order_id: Some(self.order_id().into()),
            creator_account_id: Some(self.creator_account_id().into()),
            current_tip_note_id: Some((&self.current_tip_note_id).into()),
            current_depth: self.current_depth,
            remaining_offered: self.remaining_offered.into(),
            remaining_requested: self.remaining_requested.into(),
            state: proto::PswapLineageState::from(self.state).into(),
        }
    }

    fn from_proto(record: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "pswap lineage record";

        let original_note_id =
            required(record.original_note_id, MESSAGE, "original note")?.decode_and_verify()?;
        let order_id = required(record.order_id, MESSAGE, "order id")?.try_into()?;
        let creator_account_id =
            required(record.creator_account_id, MESSAGE, "creator account")?.decode_and_verify()?;
        let current_tip_note_id =
            required(record.current_tip_note_id, MESSAGE, "current tip note")?
                .decode_and_verify()?;
        let remaining_offered = AssetAmount::new(record.remaining_offered)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
        let remaining_requested = AssetAmount::new(record.remaining_requested)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
        let state: PswapLineageState =
            proto::PswapLineageState::try_from(record.state)?.try_into()?;

        build_record_from_fields(
            original_note_id,
            order_id,
            creator_account_id,
            current_tip_note_id,
            record.current_depth,
            remaining_offered,
            remaining_requested,
            state.as_u8(),
        )
        .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

impl From<PswapLineageState> for proto::PswapLineageState {
    fn from(state: PswapLineageState) -> Self {
        match state {
            PswapLineageState::Active => Self::Active,
            PswapLineageState::FullyFilled => Self::FullyFilled,
            PswapLineageState::Reclaimed => Self::Reclaimed,
        }
    }
}

/// The unspecified value is rejected, because the stage decides whether the order can still be
/// filled.
impl TryFrom<proto::PswapLineageState> for PswapLineageState {
    type Error = ProtoDecodeError;

    fn try_from(state: proto::PswapLineageState) -> Result<Self, Self::Error> {
        match state {
            proto::PswapLineageState::Active => Ok(Self::Active),
            proto::PswapLineageState::FullyFilled => Ok(Self::FullyFilled),
            proto::PswapLineageState::Reclaimed => Ok(Self::Reclaimed),
            proto::PswapLineageState::Unspecified => Err(ProtoDecodeError::InvalidValue(
                "pswap lineage state is unspecified".to_string(),
            )),
        }
    }
}

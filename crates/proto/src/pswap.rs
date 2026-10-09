//! Protobuf conversion of PSWAP lineage records.

use std::string::ToString;

use miden_client::pswap::{PswapLineageRecord, PswapLineageState};
use miden_objects::DecodeMessageExt;
use miden_protocol::asset::AssetAmount;

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for PswapLineageRecord {
    type Message = proto::PswapLineageRecord;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            original_note_id: Some((&self.original_note_id).into()),
            order_id: Some(self.order_id().into()),
            creator_account_id: Some(self.creator_account_id().into()),
            current_tip_note_id: Some((&self.current_tip_note_id).into()),
            current_depth: self.current_depth,
            remaining_offered: self.remaining_offered.as_u64(),
            remaining_requested: self.remaining_requested.as_u64(),
            state: proto::PswapLineageState::from(self.state).into(),
        }
    }

    fn from_proto(record: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "PSWAP lineage record";

        Ok(PswapLineageRecord::from_parts(
            required(record.original_note_id, MESSAGE, "original note id")?.decode_and_verify()?,
            required(record.order_id, MESSAGE, "order id")?.try_into()?,
            required(record.creator_account_id, MESSAGE, "creator account id")?
                .decode_and_verify()?,
            required(record.current_tip_note_id, MESSAGE, "current tip note id")?
                .decode_and_verify()?,
            record.current_depth,
            asset_amount(record.remaining_offered)?,
            asset_amount(record.remaining_requested)?,
            proto::PswapLineageState::try_from(record.state)?.try_into()?,
        ))
    }
}

fn asset_amount(amount: u64) -> Result<AssetAmount, ProtoDecodeError> {
    AssetAmount::new(amount).map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
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

impl TryFrom<proto::PswapLineageState> for PswapLineageState {
    type Error = ProtoDecodeError;

    fn try_from(state: proto::PswapLineageState) -> Result<Self, Self::Error> {
        match state {
            proto::PswapLineageState::Active => Ok(Self::Active),
            proto::PswapLineageState::FullyFilled => Ok(Self::FullyFilled),
            proto::PswapLineageState::Reclaimed => Ok(Self::Reclaimed),
            proto::PswapLineageState::Unspecified => Err(ProtoDecodeError::InvalidValue(
                "PSWAP lineage state is unspecified".to_string(),
            )),
        }
    }
}

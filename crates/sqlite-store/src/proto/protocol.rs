//! The store keeps these protocol values in their own columns, as the messages that `miden-objects`
//! defines.

use std::string::ToString;
use std::vec::Vec;

use miden_objects::{DecodeMessageExt, proto as objects};
use miden_protocol::Word;
use miden_protocol::account::AccountCode;
use miden_protocol::block::BlockHeader;
use miden_protocol::note::{
    NoteAssets,
    NoteAttachments,
    NoteInclusionProof,
    NoteMetadata,
    NoteScript,
    NoteStorage,
};
use miden_protocol::protocol_config::ProtocolConfig;
use miden_protocol::transaction::TransactionScript;

use crate::proto::{self, ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for AccountCode {
    type Message = objects::account::AccountCode;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for TransactionScript {
    type Message = objects::transaction::TransactionScript;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteScript {
    type Message = objects::note::NoteScript;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteAttachments {
    type Message = objects::note::NoteAttachments;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteStorage {
    type Message = objects::note::NoteStorage;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for ProtocolConfig {
    type Message = objects::protocol_config::ProtocolConfig;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for NoteMetadata {
    type Message = objects::note::NoteMetadata;

    fn to_proto(&self) -> Self::Message {
        (*self).into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

/// The store only keeps headers that it received from the node or built from them, so a stored
/// header is not verified again when it is read.
impl ProtobufValue for BlockHeader {
    type Message = objects::blockchain::BlockHeader;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_build_unchecked()?)
    }
}

impl ProtobufValue for NoteAssets {
    type Message = proto::NoteAssets;

    fn to_proto(&self) -> Self::Message {
        proto::NoteAssets {
            assets: self.iter().map(Into::into).collect(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        let assets = message
            .assets
            .into_iter()
            .map(DecodeMessageExt::decode_and_verify)
            .collect::<Result<Vec<_>, _>>()?;
        Self::new(assets).map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

/// The peaks of the partial blockchain MMR, without their forest.
impl ProtobufValue for Vec<Word> {
    type Message = proto::MmrPeaks;

    fn to_proto(&self) -> Self::Message {
        proto::MmrPeaks {
            peaks: self.iter().map(Into::into).collect(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.peaks.into_iter().map(Word::try_from).collect::<Result<_, _>>()?)
    }
}

// NOTE INCLUSION PROOF
// ================================================================================================

impl From<&NoteInclusionProof> for proto::NoteInclusionProof {
    fn from(proof: &NoteInclusionProof) -> Self {
        Self {
            block_num: Some(proof.location().block_num().into()),
            note_index_in_block: proof.location().block_note_tree_index().into(),
            inclusion_path: Some(proof.note_path().clone().into()),
        }
    }
}

impl TryFrom<proto::NoteInclusionProof> for NoteInclusionProof {
    type Error = ProtoDecodeError;

    fn try_from(proof: proto::NoteInclusionProof) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "note inclusion proof";

        let block_num = required(proof.block_num, MESSAGE, "block number")?.decode_and_verify()?;
        let index = u16::try_from(proof.note_index_in_block).map_err(|_| {
            ProtoDecodeError::InvalidValue(format!(
                "note index {} is out of range",
                proof.note_index_in_block
            ))
        })?;
        let path =
            required(proof.inclusion_path, MESSAGE, "inclusion path")?.decode_and_verify()?;

        Self::new(block_num, index, path)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
    }
}

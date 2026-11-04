//! Protocol values. Most of them use the messages that `miden-objects` defines.

use std::string::ToString;
use std::sync::Arc;
use std::vec::Vec;

use miden_objects::{DecodeMessageExt, proto as objects};
use miden_protocol::account::{AccountCode, AccountProcedureRoot};
use miden_protocol::assembly::mast::UntrustedMastForest;
use miden_protocol::block::BlockHeader;
use miden_protocol::block::account_tree::AccountWitness;
use miden_protocol::crypto::merkle::mmr::{Forest, MmrPeaks};
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
use miden_protocol::utils::serde::{Deserializable, Serializable};
use miden_protocol::{Felt, MastForest, MastNodeId, Word};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for AccountCode {
    type Message = proto::AccountCode;

    fn to_proto(&self) -> Self::Message {
        proto::AccountCode {
            mast: Some(self.mast().as_ref().into()),
            procedure_roots: self.procedure_roots().map(Into::into).collect(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        account_code(message, true)
    }

    fn from_proto_unchecked(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        account_code(message, false)
    }
}

fn account_code(
    message: proto::AccountCode,
    checked: bool,
) -> Result<AccountCode, ProtoDecodeError> {
    let mast = read_mast(message.mast, "account code", checked)?;
    let procedures = message
        .procedure_roots
        .into_iter()
        .map(|root| Word::try_from(root).map(AccountProcedureRoot::from_raw))
        .collect::<Result<Vec<_>, _>>()?;
    AccountCode::from_parts(mast, procedures)
        .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
}

impl ProtobufValue for TransactionScript {
    type Message = proto::TransactionScript;

    fn to_proto(&self) -> Self::Message {
        proto::TransactionScript {
            mast: Some(self.mast().as_ref().into()),
            entrypoint: self.entrypoint().into(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        transaction_script(message, true)
    }

    fn from_proto_unchecked(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        transaction_script(message, false)
    }
}

fn transaction_script(
    message: proto::TransactionScript,
    checked: bool,
) -> Result<TransactionScript, ProtoDecodeError> {
    let mast = read_mast(message.mast, "transaction script", checked)?;
    let entrypoint = MastNodeId::from_u32_safe(message.entrypoint, &mast)
        .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
    TransactionScript::from_parts(mast, entrypoint)
        .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
}

impl ProtobufValue for NoteScript {
    type Message = proto::NoteScript;

    fn to_proto(&self) -> Self::Message {
        proto::NoteScript {
            mast: Some(self.mast().as_ref().into()),
            entrypoint: self.entrypoint().into(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        note_script(message, true)
    }

    fn from_proto_unchecked(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        note_script(message, false)
    }
}

fn note_script(message: proto::NoteScript, checked: bool) -> Result<NoteScript, ProtoDecodeError> {
    let mast = read_mast(message.mast, "note script", checked)?;
    let entrypoint = MastNodeId::from_u32_safe(message.entrypoint, &mast)
        .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?;
    NoteScript::from_parts(mast, entrypoint)
        .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
}

impl From<&MastForest> for proto::MastForest {
    fn from(mast: &MastForest) -> Self {
        Self { encoded: mast.to_bytes() }
    }
}

/// Reads a MAST forest. When `checked` is false, the structure and node hashes are not verified.
fn read_mast(
    mast: Option<proto::MastForest>,
    message: &'static str,
    checked: bool,
) -> Result<Arc<MastForest>, ProtoDecodeError> {
    let mast = required(mast, message, "mast")?;
    let forest = if checked {
        UntrustedMastForest::read_from_bytes(&mast.encoded)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?
            .validate()
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?
    } else {
        MastForest::read_from_bytes(&mast.encoded)
            .map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))?
    };
    Ok(Arc::new(forest))
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

impl ProtobufValue for NoteMetadata {
    type Message = objects::note::NoteMetadata;

    fn to_proto(&self) -> Self::Message {
        (*self).into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

impl ProtobufValue for AccountWitness {
    type Message = objects::account::AccountWitness;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_verify()?)
    }
}

/// A header alone does not have the data to verify its parent linkage or signatures. The caller
/// must authenticate a header from a source that is not trusted.
impl ProtobufValue for BlockHeader {
    type Message = objects::blockchain::BlockHeader;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.decode_and_build_unchecked()?)
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

impl ProtobufValue for Felt {
    type Message = objects::primitives::Felt;

    fn to_proto(&self) -> Self::Message {
        self.into()
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.try_into()?)
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

// MMR PEAKS
// ================================================================================================

/// Encodes the peaks of the partial blockchain MMR without the forest.
pub fn encode_mmr_peaks(peaks: &MmrPeaks) -> Vec<u8> {
    prost::Message::encode_to_vec(&proto::MmrPeaks {
        peaks: peaks.peaks().iter().map(Into::into).collect(),
    })
}

/// Decodes the peaks of the partial blockchain MMR. The forest comes from the checkpoint row.
pub fn decode_mmr_peaks(forest: Forest, bytes: &[u8]) -> Result<MmrPeaks, ProtoDecodeError> {
    let message = <proto::MmrPeaks as prost::Message>::decode(bytes)?;
    let peaks = message.peaks.into_iter().map(Word::try_from).collect::<Result<Vec<_>, _>>()?;
    MmrPeaks::new(forest, peaks).map_err(|err| ProtoDecodeError::InvalidValue(err.to_string()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode, encode};

    #[test]
    fn mast_values_round_trip() {
        let code = AccountCode::mock();
        assert_eq!(decode::<AccountCode>(&encode(&code)).unwrap(), code);

        let note_script = NoteScript::mock();
        assert_eq!(decode::<NoteScript>(&encode(&note_script)).unwrap(), note_script);

        let tx_script =
            TransactionScript::from_parts(note_script.mast(), note_script.entrypoint()).unwrap();
        assert_eq!(decode::<TransactionScript>(&encode(&tx_script)).unwrap(), tx_script);
    }
}

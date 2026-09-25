//! Protobuf bindings for the values the store keeps, generated from `proto/store.proto`.
//!
//! A message field is optional in protobuf, so a value the store requires arrives as `None` when
//! the row was written by a version that did not set it. [`required`] turns that into an error at
//! the point of use, and names the field it was reading.

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use miden_objects::{ConversionError, DecodeMessageExt, proto as objects};
use miden_protocol::Word;
use miden_protocol::account::AccountCode;
use miden_protocol::block::BlockHeader;
use miden_protocol::note::{NoteAttachments, NoteMetadata, NoteRecipient, NoteScript, NoteStorage};
use miden_protocol::protocol_config::ProtocolConfig;
use miden_protocol::transaction::TransactionScript;

use crate::store::OutputNoteState;

#[rustfmt::skip]
#[allow(clippy::doc_markdown, clippy::large_enum_variant, missing_docs)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/store/miden.client.store.rs"));
}
pub use generated::*;

// PROTOBUF VALUE
// ================================================================================================

/// A value that the store keeps as a protobuf message.
pub trait ProtobufValue: Sized {
    /// The message that the store writes for this value.
    type Message: prost::Message + Default;

    /// Builds the message that the store writes.
    fn to_proto(&self) -> Self::Message;

    /// Builds the value from a stored message and checks its domain constraints.
    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError>;
}

/// Encodes a value as the protobuf message that the store keeps.
pub fn encode<T: ProtobufValue>(value: &T) -> Vec<u8> {
    prost::Message::encode_to_vec(&value.to_proto())
}

/// Decodes a stored protobuf message and builds the value from it.
pub fn decode<T: ProtobufValue>(bytes: &[u8]) -> Result<T, ProtoDecodeError> {
    let message = <T::Message as prost::Message>::decode(bytes)?;
    T::from_proto(message)
}

// ERRORS
// ================================================================================================

/// Errors that occur when the store reads a protobuf value.
#[derive(Debug, thiserror::Error)]
pub enum ProtoDecodeError {
    /// The bytes are not a valid protobuf message.
    #[error("invalid protobuf message")]
    Wire(#[from] prost::DecodeError),
    /// A field that the value requires is absent.
    #[error("{message} has no {field}")]
    MissingField {
        message: &'static str,
        field: &'static str,
    },
    /// A protocol message does not describe a valid protocol value.
    #[error("invalid protocol value")]
    Conversion(#[from] ConversionError),
    /// The message is well formed, but its content breaks a rule of the value.
    #[error("invalid value: {0}")]
    InvalidValue(String),
}

// HELPERS
// ================================================================================================

/// Returns a field that the store requires, or an error that names it.
pub(crate) fn required<T>(
    field: Option<T>,
    message: &'static str,
    name: &'static str,
) -> Result<T, ProtoDecodeError> {
    field.ok_or(ProtoDecodeError::MissingField { message, field: name })
}

// NOTE INCLUSION PROOF
// ================================================================================================

impl From<&miden_protocol::note::NoteInclusionProof> for NoteInclusionProof {
    fn from(proof: &miden_protocol::note::NoteInclusionProof) -> Self {
        Self {
            block_num: Some(proof.location().block_num().into()),
            note_index_in_block: proof.location().block_note_tree_index().into(),
            inclusion_path: Some(proof.note_path().clone().into()),
        }
    }
}

impl TryFrom<NoteInclusionProof> for miden_protocol::note::NoteInclusionProof {
    type Error = ProtoDecodeError;

    fn try_from(proof: NoteInclusionProof) -> Result<Self, Self::Error> {
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

// OUTPUT NOTE STATE
// ================================================================================================

// The store keeps an output note state without the recipient's script, which lives in
// `notes_scripts`. Reading it back needs that script, so this type does not fit `ProtobufValue`.

/// Encodes an output note state as the message that the store keeps, without the script.
pub fn encode_output_note_state(state: &OutputNoteState) -> Vec<u8> {
    use stored_output_note_state::{
        CommittedFull,
        CommittedPartial,
        Consumed,
        ExpectedFull,
        ExpectedPartial,
        State,
    };

    let state = match state {
        OutputNoteState::ExpectedPartial => State::ExpectedPartial(ExpectedPartial {}),
        OutputNoteState::ExpectedFull { recipient } => State::ExpectedFull(ExpectedFull {
            recipient: Some(stored_recipient(recipient)),
        }),
        OutputNoteState::CommittedPartial { inclusion_proof } => {
            State::CommittedPartial(CommittedPartial {
                inclusion_proof: Some(inclusion_proof.into()),
            })
        },
        OutputNoteState::CommittedFull { recipient, inclusion_proof } => {
            State::CommittedFull(CommittedFull {
                recipient: Some(stored_recipient(recipient)),
                inclusion_proof: Some(inclusion_proof.into()),
            })
        },
        OutputNoteState::Consumed { block_height, recipient } => State::Consumed(Consumed {
            block_height: Some((*block_height).into()),
            recipient: Some(stored_recipient(recipient)),
        }),
    };

    prost::Message::encode_to_vec(&StoredOutputNoteState { state: Some(state) })
}

/// Decodes a stored output note state and completes its recipient with `script`, which the store
/// reads from `notes_scripts`.
pub fn decode_output_note_state(
    bytes: &[u8],
    script: Option<NoteScript>,
) -> Result<OutputNoteState, ProtoDecodeError> {
    use stored_output_note_state::State;

    const MESSAGE: &str = "stored output note state";

    let message = <StoredOutputNoteState as prost::Message>::decode(bytes)?;

    Ok(match required(message.state, MESSAGE, "variant")? {
        State::ExpectedPartial(_) => OutputNoteState::ExpectedPartial,
        State::ExpectedFull(inner) => OutputNoteState::ExpectedFull {
            recipient: full_recipient(required(inner.recipient, MESSAGE, "recipient")?, script)?,
        },
        State::CommittedPartial(inner) => OutputNoteState::CommittedPartial {
            inclusion_proof: required(inner.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
        },
        State::CommittedFull(inner) => OutputNoteState::CommittedFull {
            recipient: full_recipient(required(inner.recipient, MESSAGE, "recipient")?, script)?,
            inclusion_proof: required(inner.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
        },
        State::Consumed(inner) => OutputNoteState::Consumed {
            block_height: required(inner.block_height, MESSAGE, "block height")?
                .decode_and_verify()?,
            recipient: full_recipient(required(inner.recipient, MESSAGE, "recipient")?, script)?,
        },
    })
}

fn stored_recipient(recipient: &NoteRecipient) -> StoredNoteRecipient {
    StoredNoteRecipient {
        serial_num: Some(recipient.serial_num().into()),
        storage: Some(recipient.storage().into()),
    }
}

fn full_recipient(
    stored: StoredNoteRecipient,
    script: Option<NoteScript>,
) -> Result<NoteRecipient, ProtoDecodeError> {
    const MESSAGE: &str = "stored note recipient";

    let serial_num = required(stored.serial_num, MESSAGE, "serial number")?.try_into()?;
    let storage = required(stored.storage, MESSAGE, "storage")?.decode_and_verify()?;
    let script = script.ok_or_else(|| {
        ProtoDecodeError::InvalidValue(
            "output note state has a recipient but no script row".to_string(),
        )
    })?;

    Ok(NoteRecipient::new(serial_num, script, storage))
}

// PROTOCOL VALUES
// ================================================================================================

// The store keeps these protocol values in their own columns, as the messages that `miden-objects`
// defines.

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

impl ProtobufValue for miden_protocol::note::NoteAssets {
    type Message = NoteAssets;

    fn to_proto(&self) -> Self::Message {
        NoteAssets {
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
    type Message = MmrPeaks;

    fn to_proto(&self) -> Self::Message {
        MmrPeaks {
            peaks: self.iter().map(Into::into).collect(),
        }
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        Ok(message.peaks.into_iter().map(Word::try_from).collect::<Result<_, _>>()?)
    }
}

//! Protobuf conversion of output note states.

use std::string::ToString;
use std::vec::Vec;

use miden_client::store::OutputNoteState;
use miden_objects::DecodeMessageExt;
use miden_protocol::note::{NoteRecipient, NoteScript};

use crate as proto;
use crate::{ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for OutputNoteState {
    type Message = proto::OutputNoteState;

    fn to_proto(&self) -> Self::Message {
        output_note_state(self, true)
    }

    fn from_proto(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        output_note_state_from_proto(message, None, true)
    }

    fn from_proto_unchecked(message: Self::Message) -> Result<Self, ProtoDecodeError> {
        output_note_state_from_proto(message, None, false)
    }
}

/// Encodes an output note state without the script of its recipient.
pub fn encode_output_note_state_without_script(state: &OutputNoteState) -> Vec<u8> {
    prost::Message::encode_to_vec(&output_note_state(state, false))
}

fn output_note_state(state: &OutputNoteState, include_script: bool) -> proto::OutputNoteState {
    use proto::output_note_state::{
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
            recipient: Some(output_note_recipient(recipient, include_script)),
        }),
        OutputNoteState::CommittedPartial { inclusion_proof } => {
            State::CommittedPartial(CommittedPartial {
                inclusion_proof: Some(inclusion_proof.into()),
            })
        },
        OutputNoteState::CommittedFull { recipient, inclusion_proof } => {
            State::CommittedFull(CommittedFull {
                recipient: Some(output_note_recipient(recipient, include_script)),
                inclusion_proof: Some(inclusion_proof.into()),
            })
        },
        OutputNoteState::Consumed { block_height, recipient } => State::Consumed(Consumed {
            block_height: Some((*block_height).into()),
            recipient: Some(output_note_recipient(recipient, include_script)),
        }),
    };

    proto::OutputNoteState { state: Some(state) }
}

/// Decodes an output note state that was encoded without its script, and completes its recipient
/// with `script`.
pub fn decode_output_note_state_without_script(
    bytes: &[u8],
    script: Option<NoteScript>,
) -> Result<OutputNoteState, ProtoDecodeError> {
    let message = <proto::OutputNoteState as prost::Message>::decode(bytes)?;
    output_note_state_from_proto(message, script, true)
}

fn output_note_state_from_proto(
    message: proto::OutputNoteState,
    external_script: Option<NoteScript>,
    checked: bool,
) -> Result<OutputNoteState, ProtoDecodeError> {
    use proto::output_note_state::State;

    const MESSAGE: &str = "output note state";

    Ok(match required(message.state, MESSAGE, "variant")? {
        State::ExpectedPartial(_) => OutputNoteState::ExpectedPartial,
        State::ExpectedFull(inner) => OutputNoteState::ExpectedFull {
            recipient: output_note_recipient_from_proto(
                required(inner.recipient, MESSAGE, "recipient")?,
                external_script,
                checked,
            )?,
        },
        State::CommittedPartial(inner) => OutputNoteState::CommittedPartial {
            inclusion_proof: required(inner.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
        },
        State::CommittedFull(inner) => OutputNoteState::CommittedFull {
            recipient: output_note_recipient_from_proto(
                required(inner.recipient, MESSAGE, "recipient")?,
                external_script,
                checked,
            )?,
            inclusion_proof: required(inner.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
        },
        State::Consumed(inner) => OutputNoteState::Consumed {
            block_height: required(inner.block_height, MESSAGE, "block height")?
                .decode_and_verify()?,
            recipient: output_note_recipient_from_proto(
                required(inner.recipient, MESSAGE, "recipient")?,
                external_script,
                checked,
            )?,
        },
    })
}

fn output_note_recipient(
    recipient: &NoteRecipient,
    include_script: bool,
) -> proto::OutputNoteRecipient {
    proto::OutputNoteRecipient {
        serial_num: Some(recipient.serial_num().into()),
        storage: Some(recipient.storage().into()),
        script: include_script.then(|| recipient.script().to_proto()),
    }
}

fn output_note_recipient_from_proto(
    recipient: proto::OutputNoteRecipient,
    external_script: Option<NoteScript>,
    checked: bool,
) -> Result<NoteRecipient, ProtoDecodeError> {
    const MESSAGE: &str = "output note recipient";

    let serial_num = required(recipient.serial_num, MESSAGE, "serial number")?.try_into()?;
    let storage = required(recipient.storage, MESSAGE, "storage")?.decode_and_verify()?;
    let embedded_script = recipient
        .script
        .map(|script| {
            if checked {
                NoteScript::from_proto(script)
            } else {
                NoteScript::from_proto_unchecked(script)
            }
        })
        .transpose()?;
    let script = match (embedded_script, external_script) {
        (Some(embedded), Some(external)) if embedded != external => {
            return Err(ProtoDecodeError::InvalidValue(
                "embedded and external note scripts do not match".to_string(),
            ));
        },
        (Some(embedded), _) => embedded,
        (None, Some(external)) => external,
        (None, None) => {
            return Err(ProtoDecodeError::MissingField { message: MESSAGE, field: "script" });
        },
    };

    Ok(NoteRecipient::new(serial_num, script, storage))
}

#[cfg(test)]
mod tests {
    use std::vec;

    use miden_protocol::Word;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::{NoteInclusionProof, NoteStorage};
    use miden_standards::note::StandardNote;

    use super::*;
    use crate::{decode, encode};

    fn committed_state(script: NoteScript) -> OutputNoteState {
        let recipient =
            NoteRecipient::new(Word::empty(), script, NoteStorage::new(vec![]).unwrap());
        let path = SparseMerklePath::from_parts(0, Vec::new()).unwrap();
        let inclusion_proof = NoteInclusionProof::new(BlockNumber::from(3u32), 1, path).unwrap();
        OutputNoteState::CommittedFull { recipient, inclusion_proof }
    }

    #[test]
    fn output_note_state_round_trips_with_its_script() {
        let state = committed_state(StandardNote::P2ID.script());

        assert_eq!(decode::<OutputNoteState>(&encode(&state)).unwrap(), state);
    }

    /// An output note state is encoded without the recipient's script and decodes back with the
    /// script that the reader supplies.
    #[test]
    fn output_note_state_round_trips_without_its_script() {
        let script = StandardNote::P2ID.script();
        let state = committed_state(script.clone());

        let bytes = encode_output_note_state_without_script(&state);

        assert_eq!(decode_output_note_state_without_script(&bytes, Some(script)).unwrap(), state);
        assert!(decode_output_note_state_without_script(&bytes, None).is_err());
    }

    #[test]
    fn external_script_must_match_embedded_script() {
        let state = committed_state(StandardNote::P2ID.script());
        let bytes = encode(&state);

        assert!(
            decode_output_note_state_without_script(&bytes, Some(StandardNote::SWAP.script()))
                .is_err()
        );
    }
}

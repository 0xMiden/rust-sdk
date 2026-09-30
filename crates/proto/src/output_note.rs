//! An output note state is encoded without the recipient's script. Decoding needs that script, so
//! this type does not fit `ProtobufValue`.

use std::string::ToString;
use std::vec::Vec;

use miden_client::store::OutputNoteState;
use miden_objects::DecodeMessageExt;
use miden_protocol::note::{NoteRecipient, NoteScript};

use crate as proto;
use crate::{ProtoDecodeError, required};

/// Encodes an output note state without the script of its recipient.
pub fn encode_output_note_state_without_script(state: &OutputNoteState) -> Vec<u8> {
    use proto::output_note_state_without_script::{
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
            recipient: Some(recipient_without_script(recipient)),
        }),
        OutputNoteState::CommittedPartial { inclusion_proof } => {
            State::CommittedPartial(CommittedPartial {
                inclusion_proof: Some(inclusion_proof.into()),
            })
        },
        OutputNoteState::CommittedFull { recipient, inclusion_proof } => {
            State::CommittedFull(CommittedFull {
                recipient: Some(recipient_without_script(recipient)),
                inclusion_proof: Some(inclusion_proof.into()),
            })
        },
        OutputNoteState::Consumed { block_height, recipient } => State::Consumed(Consumed {
            block_height: Some((*block_height).into()),
            recipient: Some(recipient_without_script(recipient)),
        }),
    };

    prost::Message::encode_to_vec(&proto::OutputNoteStateWithoutScript { state: Some(state) })
}

/// Decodes an output note state that was encoded without its script, and completes its recipient
/// with `script`.
pub fn decode_output_note_state_without_script(
    bytes: &[u8],
    script: Option<NoteScript>,
) -> Result<OutputNoteState, ProtoDecodeError> {
    use proto::output_note_state_without_script::State;

    const MESSAGE: &str = "output note state without script";

    let message = <proto::OutputNoteStateWithoutScript as prost::Message>::decode(bytes)?;

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

fn recipient_without_script(recipient: &NoteRecipient) -> proto::NoteRecipientWithoutScript {
    proto::NoteRecipientWithoutScript {
        serial_num: Some(recipient.serial_num().into()),
        storage: Some(recipient.storage().into()),
    }
}

fn full_recipient(
    recipient: proto::NoteRecipientWithoutScript,
    script: Option<NoteScript>,
) -> Result<NoteRecipient, ProtoDecodeError> {
    const MESSAGE: &str = "note recipient without script";

    let serial_num = required(recipient.serial_num, MESSAGE, "serial number")?.try_into()?;
    let storage = required(recipient.storage, MESSAGE, "storage")?.decode_and_verify()?;
    let script = script.ok_or_else(|| {
        ProtoDecodeError::InvalidValue(
            "output note state has a recipient but no script".to_string(),
        )
    })?;

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

    /// An output note state is encoded without the recipient's script and decodes back with the
    /// script that the reader supplies.
    #[test]
    fn output_note_state_round_trips_without_its_script() {
        let script = StandardNote::P2ID.script();
        let recipient =
            NoteRecipient::new(Word::empty(), script.clone(), NoteStorage::new(vec![]).unwrap());
        let path = SparseMerklePath::from_parts(0, Vec::new()).unwrap();
        let inclusion_proof = NoteInclusionProof::new(BlockNumber::from(3u32), 1, path).unwrap();
        let state = OutputNoteState::CommittedFull { recipient, inclusion_proof };

        let bytes = encode_output_note_state_without_script(&state);

        assert_eq!(decode_output_note_state_without_script(&bytes, Some(script)).unwrap(), state);
        assert!(decode_output_note_state_without_script(&bytes, None).is_err());
    }
}

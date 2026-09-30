use miden_client::store::input_note_states::{
    CommittedNoteState,
    ConsumedAuthenticatedLocalNoteState,
    ConsumedExternalNoteState,
    ConsumedUnauthenticatedLocalNoteState,
    ExpectedNoteState,
    InputNoteState,
    InvalidNoteState,
    NoteSubmissionData,
    ProcessingAuthenticatedNoteState,
    ProcessingUnauthenticatedNoteState,
    UnverifiedNoteState,
};
use miden_objects::DecodeMessageExt;
use miden_protocol::note::NoteTag;

use crate::proto::{self, ProtoDecodeError, ProtobufValue, required};

impl ProtobufValue for InputNoteState {
    type Message = proto::InputNoteState;

    fn to_proto(&self) -> Self::Message {
        use proto::input_note_state::State;

        let state = match self {
            InputNoteState::Expected(inner) => State::Expected(inner.into()),
            InputNoteState::Unverified(inner) => State::Unverified(inner.into()),
            InputNoteState::Committed(inner) => State::Committed(inner.into()),
            InputNoteState::Invalid(inner) => State::Invalid(inner.into()),
            InputNoteState::ProcessingAuthenticated(inner) => {
                State::ProcessingAuthenticated(inner.into())
            },
            InputNoteState::ProcessingUnauthenticated(inner) => {
                State::ProcessingUnauthenticated(inner.into())
            },
            InputNoteState::ConsumedAuthenticatedLocal(inner) => {
                State::ConsumedAuthenticatedLocal(inner.into())
            },
            InputNoteState::ConsumedUnauthenticatedLocal(inner) => {
                State::ConsumedUnauthenticatedLocal(inner.into())
            },
            InputNoteState::ConsumedExternal(inner) => State::ConsumedExternal(inner.into()),
        };

        Self::Message { state: Some(state) }
    }

    fn from_proto(state: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::input_note_state::State;

        let state = required(state.state, "input note state", "variant")?;

        Ok(match state {
            State::Expected(inner) => ExpectedNoteState::try_from(inner)?.into(),
            State::Unverified(inner) => UnverifiedNoteState::try_from(inner)?.into(),
            State::Committed(inner) => CommittedNoteState::try_from(inner)?.into(),
            State::Invalid(inner) => InvalidNoteState::try_from(inner)?.into(),
            State::ProcessingAuthenticated(inner) => {
                ProcessingAuthenticatedNoteState::try_from(inner)?.into()
            },
            State::ProcessingUnauthenticated(inner) => {
                ProcessingUnauthenticatedNoteState::try_from(inner)?.into()
            },
            State::ConsumedAuthenticatedLocal(inner) => {
                ConsumedAuthenticatedLocalNoteState::try_from(inner)?.into()
            },
            State::ConsumedUnauthenticatedLocal(inner) => {
                ConsumedUnauthenticatedLocalNoteState::try_from(inner)?.into()
            },
            State::ConsumedExternal(inner) => ConsumedExternalNoteState::try_from(inner)?.into(),
        })
    }
}

// STATES
// ================================================================================================

impl From<&ExpectedNoteState> for proto::input_note_state::Expected {
    fn from(state: &ExpectedNoteState) -> Self {
        Self {
            metadata: state.metadata.map(Into::into),
            after_block_num: Some(state.after_block_num.into()),
            tag: state.tag.map(Into::into),
        }
    }
}

impl TryFrom<proto::input_note_state::Expected> for ExpectedNoteState {
    type Error = ProtoDecodeError;

    fn try_from(state: proto::input_note_state::Expected) -> Result<Self, Self::Error> {
        Ok(ExpectedNoteState {
            metadata: state.metadata.map(DecodeMessageExt::decode_and_verify).transpose()?,
            after_block_num: required(
                state.after_block_num,
                "expected note state",
                "after block number",
            )?
            .decode_and_verify()?,
            tag: state.tag.map(NoteTag::from),
        })
    }
}

impl From<&UnverifiedNoteState> for proto::input_note_state::Unverified {
    fn from(state: &UnverifiedNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            inclusion_proof: Some((&state.inclusion_proof).into()),
        }
    }
}

impl TryFrom<proto::input_note_state::Unverified> for UnverifiedNoteState {
    type Error = ProtoDecodeError;

    fn try_from(state: proto::input_note_state::Unverified) -> Result<Self, Self::Error> {
        Ok(UnverifiedNoteState {
            metadata: required(state.metadata, "unverified note state", "metadata")?
                .decode_and_verify()?,
            inclusion_proof: required(
                state.inclusion_proof,
                "unverified note state",
                "inclusion proof",
            )?
            .try_into()?,
        })
    }
}

impl From<&CommittedNoteState> for proto::input_note_state::Committed {
    fn from(state: &CommittedNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            inclusion_proof: Some((&state.inclusion_proof).into()),
            block_note_root: Some(state.block_note_root.into()),
        }
    }
}

impl TryFrom<proto::input_note_state::Committed> for CommittedNoteState {
    type Error = ProtoDecodeError;

    fn try_from(state: proto::input_note_state::Committed) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "committed note state";

        Ok(CommittedNoteState {
            metadata: required(state.metadata, MESSAGE, "metadata")?.decode_and_verify()?,
            inclusion_proof: required(state.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
            block_note_root: required(state.block_note_root, MESSAGE, "block note root")?
                .try_into()?,
        })
    }
}

impl From<&InvalidNoteState> for proto::input_note_state::Invalid {
    fn from(state: &InvalidNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            invalid_inclusion_proof: Some((&state.invalid_inclusion_proof).into()),
            block_note_root: Some(state.block_note_root.into()),
        }
    }
}

impl TryFrom<proto::input_note_state::Invalid> for InvalidNoteState {
    type Error = ProtoDecodeError;

    fn try_from(state: proto::input_note_state::Invalid) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "invalid note state";

        Ok(InvalidNoteState {
            metadata: required(state.metadata, MESSAGE, "metadata")?.decode_and_verify()?,
            invalid_inclusion_proof: required(
                state.invalid_inclusion_proof,
                MESSAGE,
                "invalid inclusion proof",
            )?
            .try_into()?,
            block_note_root: required(state.block_note_root, MESSAGE, "block note root")?
                .try_into()?,
        })
    }
}

impl From<&ProcessingAuthenticatedNoteState> for proto::input_note_state::ProcessingAuthenticated {
    fn from(state: &ProcessingAuthenticatedNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            inclusion_proof: Some((&state.inclusion_proof).into()),
            block_note_root: Some(state.block_note_root.into()),
            submission_data: Some((&state.submission_data).into()),
        }
    }
}

impl TryFrom<proto::input_note_state::ProcessingAuthenticated>
    for ProcessingAuthenticatedNoteState
{
    type Error = ProtoDecodeError;

    fn try_from(
        state: proto::input_note_state::ProcessingAuthenticated,
    ) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "processing authenticated note state";

        Ok(ProcessingAuthenticatedNoteState {
            metadata: required(state.metadata, MESSAGE, "metadata")?.decode_and_verify()?,
            inclusion_proof: required(state.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
            block_note_root: required(state.block_note_root, MESSAGE, "block note root")?
                .try_into()?,
            submission_data: required(state.submission_data, MESSAGE, "submission data")?
                .try_into()?,
        })
    }
}

impl From<&ProcessingUnauthenticatedNoteState>
    for proto::input_note_state::ProcessingUnauthenticated
{
    fn from(state: &ProcessingUnauthenticatedNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            after_block_num: Some(state.after_block_num.into()),
            submission_data: Some((&state.submission_data).into()),
        }
    }
}

impl TryFrom<proto::input_note_state::ProcessingUnauthenticated>
    for ProcessingUnauthenticatedNoteState
{
    type Error = ProtoDecodeError;

    fn try_from(
        state: proto::input_note_state::ProcessingUnauthenticated,
    ) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "processing unauthenticated note state";

        Ok(ProcessingUnauthenticatedNoteState {
            metadata: required(state.metadata, MESSAGE, "metadata")?.decode_and_verify()?,
            after_block_num: required(state.after_block_num, MESSAGE, "after block number")?
                .decode_and_verify()?,
            submission_data: required(state.submission_data, MESSAGE, "submission data")?
                .try_into()?,
        })
    }
}

impl From<&ConsumedAuthenticatedLocalNoteState>
    for proto::input_note_state::ConsumedAuthenticatedLocal
{
    fn from(state: &ConsumedAuthenticatedLocalNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            inclusion_proof: Some((&state.inclusion_proof).into()),
            block_note_root: Some(state.block_note_root.into()),
            nullifier_block_height: Some(state.nullifier_block_height.into()),
            submission_data: Some((&state.submission_data).into()),
            consumed_tx_order: state.consumed_tx_order,
        }
    }
}

impl TryFrom<proto::input_note_state::ConsumedAuthenticatedLocal>
    for ConsumedAuthenticatedLocalNoteState
{
    type Error = ProtoDecodeError;

    fn try_from(
        state: proto::input_note_state::ConsumedAuthenticatedLocal,
    ) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "consumed authenticated local note state";

        Ok(ConsumedAuthenticatedLocalNoteState {
            metadata: required(state.metadata, MESSAGE, "metadata")?.decode_and_verify()?,
            inclusion_proof: required(state.inclusion_proof, MESSAGE, "inclusion proof")?
                .try_into()?,
            block_note_root: required(state.block_note_root, MESSAGE, "block note root")?
                .try_into()?,
            nullifier_block_height: required(
                state.nullifier_block_height,
                MESSAGE,
                "nullifier block height",
            )?
            .decode_and_verify()?,
            submission_data: required(state.submission_data, MESSAGE, "submission data")?
                .try_into()?,
            consumed_tx_order: state.consumed_tx_order,
        })
    }
}

impl From<&ConsumedUnauthenticatedLocalNoteState>
    for proto::input_note_state::ConsumedUnauthenticatedLocal
{
    fn from(state: &ConsumedUnauthenticatedLocalNoteState) -> Self {
        Self {
            metadata: Some(state.metadata.into()),
            nullifier_block_height: Some(state.nullifier_block_height.into()),
            submission_data: Some((&state.submission_data).into()),
            consumed_tx_order: state.consumed_tx_order,
        }
    }
}

impl TryFrom<proto::input_note_state::ConsumedUnauthenticatedLocal>
    for ConsumedUnauthenticatedLocalNoteState
{
    type Error = ProtoDecodeError;

    fn try_from(
        state: proto::input_note_state::ConsumedUnauthenticatedLocal,
    ) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "consumed unauthenticated local note state";

        Ok(ConsumedUnauthenticatedLocalNoteState {
            metadata: required(state.metadata, MESSAGE, "metadata")?.decode_and_verify()?,
            nullifier_block_height: required(
                state.nullifier_block_height,
                MESSAGE,
                "nullifier block height",
            )?
            .decode_and_verify()?,
            submission_data: required(state.submission_data, MESSAGE, "submission data")?
                .try_into()?,
            consumed_tx_order: state.consumed_tx_order,
        })
    }
}

impl From<&ConsumedExternalNoteState> for proto::input_note_state::ConsumedExternal {
    fn from(state: &ConsumedExternalNoteState) -> Self {
        Self {
            nullifier_block_height: Some(state.nullifier_block_height.into()),
            consumer_account: state.consumer_account.map(Into::into),
            consumed_tx_order: state.consumed_tx_order,
            metadata: state.metadata.map(Into::into),
        }
    }
}

impl TryFrom<proto::input_note_state::ConsumedExternal> for ConsumedExternalNoteState {
    type Error = ProtoDecodeError;

    fn try_from(state: proto::input_note_state::ConsumedExternal) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "consumed external note state";

        Ok(ConsumedExternalNoteState {
            nullifier_block_height: required(
                state.nullifier_block_height,
                MESSAGE,
                "nullifier block height",
            )?
            .decode_and_verify()?,
            consumer_account: state
                .consumer_account
                .map(DecodeMessageExt::decode_and_verify)
                .transpose()?,
            consumed_tx_order: state.consumed_tx_order,
            metadata: state.metadata.map(DecodeMessageExt::decode_and_verify).transpose()?,
        })
    }
}

// SUBMISSION DATA
// ================================================================================================

impl From<&NoteSubmissionData> for proto::NoteSubmissionData {
    fn from(data: &NoteSubmissionData) -> Self {
        Self {
            submitted_at: data.submitted_at,
            consumer_account: Some(data.consumer_account.into()),
            consumer_transaction: Some(data.consumer_transaction.into()),
        }
    }
}

impl TryFrom<proto::NoteSubmissionData> for NoteSubmissionData {
    type Error = ProtoDecodeError;

    fn try_from(data: proto::NoteSubmissionData) -> Result<Self, Self::Error> {
        const MESSAGE: &str = "note submission data";

        Ok(NoteSubmissionData {
            submitted_at: data.submitted_at,
            consumer_account: required(data.consumer_account, MESSAGE, "consumer account")?
                .decode_and_verify()?,
            consumer_transaction: required(
                data.consumer_transaction,
                MESSAGE,
                "consumer transaction",
            )?
            .decode_and_verify()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::vec::Vec;

    use miden_protocol::Word;
    use miden_protocol::account::AccountId;
    use miden_protocol::block::BlockNumber;
    use miden_protocol::crypto::merkle::SparseMerklePath;
    use miden_protocol::note::{
        NoteAttachments,
        NoteInclusionProof,
        NoteMetadata,
        NoteType,
        PartialNoteMetadata,
    };
    use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;
    use miden_protocol::transaction::TransactionId;

    use super::*;
    use crate::proto::{decode, encode};

    /// The conversions to and from protobuf are written field by field, so a round trip through
    /// every variant catches a field that is dropped or read into the wrong place.
    #[test]
    fn every_input_note_state_round_trips_through_protobuf() {
        let account = AccountId::try_from(ACCOUNT_ID_SENDER).unwrap();
        let metadata = NoteMetadata::new(
            PartialNoteMetadata::new(account, NoteType::Public),
            &NoteAttachments::empty(),
        );
        let path = SparseMerklePath::from_parts(0, Vec::new()).unwrap();
        let proof = NoteInclusionProof::new(BlockNumber::from(3u32), 1, path).unwrap();
        let root = Word::empty();
        let submission = NoteSubmissionData {
            submitted_at: Some(10),
            consumer_account: account,
            consumer_transaction: TransactionId::from_raw(Word::empty()),
        };
        // Distinct block numbers, so a swap between two of them fails the comparison.
        let after = BlockNumber::from(2u32);
        let nullified = BlockNumber::from(4u32);

        let states: [InputNoteState; 9] = [
            ExpectedNoteState {
                metadata: Some(metadata),
                after_block_num: after,
                tag: Some(metadata.tag()),
            }
            .into(),
            UnverifiedNoteState { metadata, inclusion_proof: proof.clone() }.into(),
            CommittedNoteState {
                metadata,
                inclusion_proof: proof.clone(),
                block_note_root: root,
            }
            .into(),
            InvalidNoteState {
                metadata,
                invalid_inclusion_proof: proof.clone(),
                block_note_root: root,
            }
            .into(),
            ProcessingAuthenticatedNoteState {
                metadata,
                inclusion_proof: proof.clone(),
                block_note_root: root,
                submission_data: submission,
            }
            .into(),
            ProcessingUnauthenticatedNoteState {
                metadata,
                after_block_num: after,
                submission_data: submission,
            }
            .into(),
            ConsumedAuthenticatedLocalNoteState {
                metadata,
                inclusion_proof: proof,
                block_note_root: root,
                nullifier_block_height: nullified,
                submission_data: submission,
                consumed_tx_order: Some(1),
            }
            .into(),
            ConsumedUnauthenticatedLocalNoteState {
                metadata,
                nullifier_block_height: nullified,
                submission_data: submission,
                consumed_tx_order: Some(1),
            }
            .into(),
            ConsumedExternalNoteState {
                nullifier_block_height: nullified,
                consumer_account: Some(account),
                consumed_tx_order: Some(1),
                metadata: Some(metadata),
            }
            .into(),
        ];

        for state in states {
            assert_eq!(decode::<InputNoteState>(&encode(&state)).unwrap(), state);
        }
    }
}

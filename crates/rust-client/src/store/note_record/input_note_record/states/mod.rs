use alloc::string::ToString;
use core::fmt::{self, Display};

use chrono::{Local, TimeZone};
use miden_protocol::account::AccountId;
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{NoteId, NoteInclusionProof, NoteMetadata};
use miden_protocol::transaction::TransactionId;
pub use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

mod committed;
mod consumed_authenticated_local;
mod consumed_external;
mod consumed_unauthenticated_local;
mod expected;
mod invalid;
mod processing_authenticated;
mod processing_unauthenticated;
mod unverified;

pub use committed::CommittedNoteState;
pub use consumed_authenticated_local::ConsumedAuthenticatedLocalNoteState;
pub use consumed_external::ConsumedExternalNoteState;
pub use consumed_unauthenticated_local::ConsumedUnauthenticatedLocalNoteState;
pub use expected::ExpectedNoteState;
pub use invalid::InvalidNoteState;
pub use processing_authenticated::ProcessingAuthenticatedNoteState;
pub use processing_unauthenticated::ProcessingUnauthenticatedNoteState;
pub use unverified::UnverifiedNoteState;

use super::NoteRecordError;

#[derive(Clone, Debug, PartialEq)]

/// The possible states of a tracked note.
pub enum InputNoteState {
    /// Tracked by the client but without a network inclusion proof.
    Expected(ExpectedNoteState),
    /// The store holds the note's inclusion proof, but it was not yet verified.
    Unverified(UnverifiedNoteState),
    /// The store holds the note's inclusion proof, which was verified.
    Committed(CommittedNoteState),
    /// The store holds the note's inclusion proof, which is invalid.
    Invalid(InvalidNoteState),
    /// Authenticated note being consumed locally by the client, awaiting network confirmation.
    ProcessingAuthenticated(ProcessingAuthenticatedNoteState),
    /// Unauthenticated note being consumed locally by the client, awaiting network confirmation.
    ProcessingUnauthenticated(ProcessingUnauthenticatedNoteState),
    /// Authenticated note consumed locally by the client and confirmed by the network.
    ConsumedAuthenticatedLocal(ConsumedAuthenticatedLocalNoteState),
    /// Unauthenticated note consumed locally by the client and confirmed by the network.
    ConsumedUnauthenticatedLocal(ConsumedUnauthenticatedLocalNoteState),
    /// Note consumed by a transaction not submitted by this client and confirmed by the network.
    ConsumedExternal(ConsumedExternalNoteState),
}

impl InputNoteState {
    pub const STATE_EXPECTED: u8 = 0;
    pub const STATE_UNVERIFIED: u8 = 1;
    pub const STATE_COMMITTED: u8 = 2;
    pub const STATE_INVALID: u8 = 3;
    pub const STATE_PROCESSING_AUTHENTICATED: u8 = 4;
    pub const STATE_PROCESSING_UNAUTHENTICATED: u8 = 5;
    pub const STATE_CONSUMED_AUTHENTICATED_LOCAL: u8 = 6;
    pub const STATE_CONSUMED_UNAUTHENTICATED_LOCAL: u8 = 7;
    pub const STATE_CONSUMED_EXTERNAL: u8 = 8;

    /// Returns the inner state handler that implements state transitions.
    fn inner(&self) -> &dyn NoteStateHandler {
        match self {
            InputNoteState::Expected(inner) => inner,
            InputNoteState::Unverified(inner) => inner,
            InputNoteState::Committed(inner) => inner,
            InputNoteState::Invalid(inner) => inner,
            InputNoteState::ProcessingAuthenticated(inner) => inner,
            InputNoteState::ProcessingUnauthenticated(inner) => inner,
            InputNoteState::ConsumedAuthenticatedLocal(inner) => inner,
            InputNoteState::ConsumedUnauthenticatedLocal(inner) => inner,
            InputNoteState::ConsumedExternal(inner) => inner,
        }
    }

    /// Returns a unique identifier for each note state.
    pub fn discriminant(&self) -> u8 {
        match self {
            InputNoteState::Expected(_) => Self::STATE_EXPECTED,
            InputNoteState::Unverified(_) => Self::STATE_UNVERIFIED,
            InputNoteState::Committed(_) => Self::STATE_COMMITTED,
            InputNoteState::Invalid(_) => Self::STATE_INVALID,
            InputNoteState::ProcessingAuthenticated(_) => Self::STATE_PROCESSING_AUTHENTICATED,
            InputNoteState::ProcessingUnauthenticated(_) => Self::STATE_PROCESSING_UNAUTHENTICATED,
            InputNoteState::ConsumedAuthenticatedLocal(_) => {
                Self::STATE_CONSUMED_AUTHENTICATED_LOCAL
            },
            InputNoteState::ConsumedUnauthenticatedLocal(_) => {
                Self::STATE_CONSUMED_UNAUTHENTICATED_LOCAL
            },
            InputNoteState::ConsumedExternal(_) => Self::STATE_CONSUMED_EXTERNAL,
        }
    }

    pub(crate) fn metadata(&self) -> Option<&NoteMetadata> {
        self.inner().metadata()
    }

    pub(crate) fn inclusion_proof(&self) -> Option<&NoteInclusionProof> {
        self.inner().inclusion_proof()
    }

    pub(crate) fn consumer_transaction_id(&self) -> Option<&TransactionId> {
        self.inner().consumer_transaction_id()
    }

    /// Returns the block height at which this note was consumed, if it is in a consumed state.
    pub fn consumed_block_height(&self) -> Option<BlockNumber> {
        match self {
            InputNoteState::ConsumedAuthenticatedLocal(s) => Some(s.nullifier_block_height),
            InputNoteState::ConsumedUnauthenticatedLocal(s) => Some(s.nullifier_block_height),
            InputNoteState::ConsumedExternal(s) => Some(s.nullifier_block_height),
            _ => None,
        }
    }

    /// Returns the per-account position of the consuming transaction within the account's
    /// execution chain for the block, if available.
    pub fn consumed_tx_order(&self) -> Option<u32> {
        match self {
            InputNoteState::ConsumedAuthenticatedLocal(s) => s.consumed_tx_order,
            InputNoteState::ConsumedUnauthenticatedLocal(s) => s.consumed_tx_order,
            InputNoteState::ConsumedExternal(s) => s.consumed_tx_order,
            _ => None,
        }
    }

    /// Sets the consumed transaction order on the inner consumed state. No-op if the note is
    /// not in a consumed state.
    pub(crate) fn set_consumed_tx_order(&mut self, order: Option<u32>) {
        match self {
            InputNoteState::ConsumedAuthenticatedLocal(s) => s.consumed_tx_order = order,
            InputNoteState::ConsumedUnauthenticatedLocal(s) => s.consumed_tx_order = order,
            InputNoteState::ConsumedExternal(s) => s.consumed_tx_order = order,
            _ => {},
        }
    }

    /// Returns a new state to reflect that the note has received an inclusion proof. The proof is
    /// assumed to be unverified until the block header information is received. If the note state
    /// doesn't change, `None` is returned.
    pub(crate) fn inclusion_proof_received(
        &self,
        inclusion_proof: NoteInclusionProof,
        metadata: NoteMetadata,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        self.inner().inclusion_proof_received(inclusion_proof, metadata)
    }

    /// Returns a new state to reflect that the note has been consumed by a transaction not
    /// submitted by this client. If the note state doesn't change, `None` is returned.
    pub(crate) fn consumed_externally(
        &self,
        nullifier_block_height: BlockNumber,
        consumer_account: Option<AccountId>,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        self.inner().consumed_externally(nullifier_block_height, consumer_account)
    }

    /// Returns a new state to reflect that the note has received a block header.
    /// This will mark the note as verified or invalid, depending on the block header
    /// information and inclusion proof. If the note state doesn't change, `None` is returned.
    pub(crate) fn block_header_received(
        &self,
        note_id: NoteId,
        block_header: &BlockHeader,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        self.inner().block_header_received(note_id, block_header)
    }

    /// Modifies the state of the note record to reflect that the client began processing the note
    /// to be consumed. If the note state doesn't change, `None` is returned.
    pub(crate) fn consumed_locally(
        &self,
        consumer_account: AccountId,
        consumer_transaction: TransactionId,
        current_timestamp: Option<u64>,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        self.inner()
            .consumed_locally(consumer_account, consumer_transaction, current_timestamp)
    }

    /// Returns a new state to reflect that the transaction currently consuming the note was
    /// committed. If the note state doesn't change, `None` is returned.
    pub(crate) fn transaction_committed(
        &self,
        transaction_id: TransactionId,
        block_height: BlockNumber,
    ) -> Result<Option<InputNoteState>, NoteRecordError> {
        self.inner().transaction_committed(transaction_id, block_height)
    }
}

impl Serializable for InputNoteState {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        target.write_u8(self.discriminant());
        match self {
            InputNoteState::Expected(inner) => inner.write_into(target),
            InputNoteState::Unverified(inner) => inner.write_into(target),
            InputNoteState::Committed(inner) => inner.write_into(target),
            InputNoteState::Invalid(inner) => inner.write_into(target),
            InputNoteState::ProcessingAuthenticated(inner) => inner.write_into(target),
            InputNoteState::ProcessingUnauthenticated(inner) => inner.write_into(target),
            InputNoteState::ConsumedAuthenticatedLocal(inner) => inner.write_into(target),
            InputNoteState::ConsumedUnauthenticatedLocal(inner) => inner.write_into(target),
            InputNoteState::ConsumedExternal(inner) => inner.write_into(target),
        }
    }
}

impl Deserializable for InputNoteState {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let discriminant = source.read_u8()?;
        match discriminant {
            Self::STATE_EXPECTED => Ok(ExpectedNoteState::read_from(source)?.into()),
            Self::STATE_UNVERIFIED => Ok(UnverifiedNoteState::read_from(source)?.into()),
            Self::STATE_COMMITTED => Ok(CommittedNoteState::read_from(source)?.into()),
            Self::STATE_INVALID => Ok(InvalidNoteState::read_from(source)?.into()),
            Self::STATE_PROCESSING_AUTHENTICATED => {
                Ok(ProcessingAuthenticatedNoteState::read_from(source)?.into())
            },
            Self::STATE_PROCESSING_UNAUTHENTICATED => {
                Ok(ProcessingUnauthenticatedNoteState::read_from(source)?.into())
            },
            Self::STATE_CONSUMED_AUTHENTICATED_LOCAL => {
                Ok(ConsumedAuthenticatedLocalNoteState::read_from(source)?.into())
            },
            Self::STATE_CONSUMED_UNAUTHENTICATED_LOCAL => {
                Ok(ConsumedUnauthenticatedLocalNoteState::read_from(source)?.into())
            },
            Self::STATE_CONSUMED_EXTERNAL => {
                Ok(ConsumedExternalNoteState::read_from(source)?.into())
            },
            _ => Err(DeserializationError::InvalidValue(format!(
                "Invalid NoteState discriminant: {discriminant}"
            ))),
        }
    }
}

impl Display for InputNoteState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InputNoteState::Expected(state) => {
                write!(f, "Expected (after block {})", state.after_block_num)
            },
            InputNoteState::Unverified(state) => {
                write!(
                    f,
                    "Unverified (with commit block {})",
                    state.inclusion_proof.location().block_num()
                )
            },
            InputNoteState::Committed(state) => {
                write!(
                    f,
                    "Committed (at block height {})",
                    state.inclusion_proof.location().block_num()
                )
            },
            InputNoteState::Invalid(state) => {
                write!(
                    f,
                    "Invalid (with commit block {})",
                    state.invalid_inclusion_proof.location().block_num()
                )
            },
            InputNoteState::ProcessingAuthenticated(ProcessingAuthenticatedNoteState {
                submission_data,
                ..
            })
            | InputNoteState::ProcessingUnauthenticated(ProcessingUnauthenticatedNoteState {
                submission_data,
                ..
            }) => {
                write!(
                    f,
                    "Processing (submitted at {} by account {})",
                    submission_data.submitted_at.map_or("?".to_string(), |submitted_at| {
                        Local
                            .timestamp_opt(
                                i64::try_from(submitted_at)
                                    .expect("i64::MAX as timestamp is year 2262"),
                                0,
                            )
                            .single()
                            .expect("timestamp should be valid")
                            .to_string()
                    }),
                    submission_data.consumer_account
                )
            },
            InputNoteState::ConsumedAuthenticatedLocal(ConsumedAuthenticatedLocalNoteState {
                nullifier_block_height,
                submission_data,
                ..
            })
            | InputNoteState::ConsumedUnauthenticatedLocal(
                ConsumedUnauthenticatedLocalNoteState {
                    nullifier_block_height,
                    submission_data,
                    ..
                },
            ) => {
                write!(
                    f,
                    "Consumed (at block {} by account {})",
                    nullifier_block_height, submission_data.consumer_account
                )
            },
            InputNoteState::ConsumedExternal(state) => {
                if let Some(account) = state.consumer_account {
                    write!(
                        f,
                        "Consumed (at block {} by tracked account {})",
                        state.nullifier_block_height, account
                    )
                } else {
                    write!(f, "Consumed (at block {})", state.nullifier_block_height)
                }
            },
        }
    }
}

pub trait NoteStateHandler {
    fn metadata(&self) -> Option<&NoteMetadata>;

    fn inclusion_proof(&self) -> Option<&NoteInclusionProof>;

    fn consumer_transaction_id(&self) -> Option<&TransactionId>;

    fn inclusion_proof_received(
        &self,
        inclusion_proof: NoteInclusionProof,
        metadata: NoteMetadata,
    ) -> Result<Option<InputNoteState>, NoteRecordError>;

    fn consumed_externally(
        &self,
        nullifier_block_height: BlockNumber,
        consumer_account: Option<AccountId>,
    ) -> Result<Option<InputNoteState>, NoteRecordError>;

    fn block_header_received(
        &self,
        note_id: NoteId,
        block_header: &BlockHeader,
    ) -> Result<Option<InputNoteState>, NoteRecordError>;

    fn consumed_locally(
        &self,
        consumer_account: AccountId,
        consumer_transaction: TransactionId,
        current_timestamp: Option<u64>,
    ) -> Result<Option<InputNoteState>, NoteRecordError>;

    fn transaction_committed(
        &self,
        transaction_id: TransactionId,
        block_height: BlockNumber,
    ) -> Result<Option<InputNoteState>, NoteRecordError>;
}

/// Information about a locally consumed note submitted to the node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct NoteSubmissionData {
    /// The timestamp at which the note was submitted.
    pub submitted_at: Option<u64>,
    /// The ID of the account that is consuming the note.
    pub consumer_account: AccountId,
    /// The ID of the transaction that is consuming the note.
    pub consumer_transaction: TransactionId,
}

impl Serializable for NoteSubmissionData {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.submitted_at.write_into(target);
        self.consumer_account.write_into(target);
        self.consumer_transaction.write_into(target);
    }
}

impl Deserializable for NoteSubmissionData {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let submitted_at = Option::<u64>::read_from(source)?;
        let consumer_account = AccountId::read_from(source)?;
        let consumer_transaction = TransactionId::read_from(source)?;
        Ok(NoteSubmissionData {
            submitted_at,
            consumer_account,
            consumer_transaction,
        })
    }
}

use alloc::format;
use alloc::string::ToString;
use alloc::vec::Vec;
use core::fmt;

use miden_objects::DecodeMessageExt;
use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::transaction::{RawOutputNotes, TransactionId, TransactionScript};
use miden_tx::utils::serde::{
    ByteReader,
    ByteWriter,
    Deserializable,
    DeserializationError,
    Serializable,
};

use crate::store::proto::{self, ProtoDecodeError, ProtobufValue};

// TRANSACTION RECORD
// ================================================================================================

/// Describes a transaction that has been executed and is being tracked on the Client.
#[derive(Debug, Clone)]
pub struct TransactionRecord {
    /// Unique identifier for the transaction.
    pub id: TransactionId,
    /// Details associated with the transaction.
    pub details: TransactionDetails,
    /// Script associated with the transaction, if no script is provided, only note scripts are
    /// executed.
    pub script: Option<TransactionScript>,
    /// Current status of the transaction.
    pub status: TransactionStatus,
}

impl TransactionRecord {
    /// Creates a new [`TransactionRecord`] instance.
    pub fn new(
        id: TransactionId,
        details: TransactionDetails,
        script: Option<TransactionScript>,
        status: TransactionStatus,
    ) -> TransactionRecord {
        TransactionRecord { id, details, script, status }
    }

    /// Updates (if necessary) the transaction status to signify that the transaction was committed.
    /// Will return true if the record was modified, false otherwise.
    pub fn commit_transaction(
        &mut self,
        commit_height: BlockNumber,
        commit_timestamp: u64,
    ) -> bool {
        match self.status {
            TransactionStatus::Pending => {
                self.status = TransactionStatus::Committed {
                    block_number: commit_height,
                    commit_timestamp,
                };
                true
            },
            // TODO: We need a better strategy here. If a transaction was discarded within this same
            // chain of updates, it would be better to pass the state to committed and then remove
            // the account invalid states and make them valid again
            TransactionStatus::Discarded(_) | TransactionStatus::Committed { .. } => false,
        }
    }

    /// Updates (if necessary) the transaction status to signify that the transaction was discarded.
    /// Will return true if the record was modified, false otherwise.
    pub fn discard_transaction(&mut self, cause: DiscardCause) -> bool {
        match self.status {
            TransactionStatus::Pending => {
                self.status = TransactionStatus::Discarded(cause);
                true
            },
            TransactionStatus::Discarded(_) | TransactionStatus::Committed { .. } => false,
        }
    }
}

/// Describes the details associated with a transaction.
#[derive(Debug, Clone)]
pub struct TransactionDetails {
    /// ID of the account that executed the transaction.
    pub account_id: AccountId,
    /// Initial state of the account before the transaction was executed.
    pub init_account_state: Word,
    /// Final state of the account after the transaction was executed.
    pub final_account_state: Word,
    /// Nullifiers of the input notes consumed in the transaction.
    pub input_note_nullifiers: Vec<Word>,
    /// Output notes generated as a result of the transaction.
    pub output_notes: RawOutputNotes,
    /// Block number for the block against which the transaction was executed.
    pub block_num: BlockNumber,
    /// Block number at which the transaction was submitted.
    pub submission_height: BlockNumber,
    /// Block number at which the transaction is set to expire.
    pub expiration_block_num: BlockNumber,
    /// Timestamp indicating when the transaction was created by the client.
    pub creation_timestamp: u64,
}

impl Serializable for TransactionDetails {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        self.account_id.write_into(target);
        self.init_account_state.write_into(target);
        self.final_account_state.write_into(target);
        self.input_note_nullifiers.write_into(target);
        self.output_notes.write_into(target);
        self.block_num.write_into(target);
        self.submission_height.write_into(target);
        self.expiration_block_num.write_into(target);
        self.creation_timestamp.write_into(target);
    }
}

impl Deserializable for TransactionDetails {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let account_id = AccountId::read_from(source)?;
        let init_account_state = Word::read_from(source)?;
        let final_account_state = Word::read_from(source)?;
        let input_note_nullifiers = Vec::<Word>::read_from(source)?;
        let output_notes = RawOutputNotes::read_from(source)?;
        let block_num = BlockNumber::read_from(source)?;
        let submission_height = BlockNumber::read_from(source)?;
        let expiration_block_num = BlockNumber::read_from(source)?;
        let creation_timestamp = source.read_u64()?;

        Ok(Self {
            account_id,
            init_account_state,
            final_account_state,
            input_note_nullifiers,
            output_notes,
            block_num,
            submission_height,
            expiration_block_num,
            creation_timestamp,
        })
    }
}

impl ProtobufValue for TransactionDetails {
    type Message = proto::TransactionDetails;

    fn to_proto(&self) -> Self::Message {
        Self::Message {
            account_id: Some(self.account_id.into()),
            init_account_state: Some(self.init_account_state.into()),
            final_account_state: Some(self.final_account_state.into()),
            input_note_nullifiers: self
                .input_note_nullifiers
                .iter()
                .map(|nullifier| (*nullifier).into())
                .collect(),
            output_notes: Some((&self.output_notes).into()),
            block_num: Some(self.block_num.into()),
            submission_height: Some(self.submission_height.into()),
            expiration_block_num: Some(self.expiration_block_num.into()),
            creation_timestamp: self.creation_timestamp,
        }
    }

    fn from_proto(details: Self::Message) -> Result<Self, ProtoDecodeError> {
        const MESSAGE: &str = "transaction details";

        let input_note_nullifiers = details
            .input_note_nullifiers
            .into_iter()
            .map(Word::try_from)
            .collect::<Result<Vec<Word>, _>>()?;

        Ok(Self {
            account_id: proto::required(details.account_id, MESSAGE, "account id")?
                .decode_and_verify()?,
            init_account_state: proto::required(
                details.init_account_state,
                MESSAGE,
                "initial account state",
            )?
            .try_into()?,
            final_account_state: proto::required(
                details.final_account_state,
                MESSAGE,
                "final account state",
            )?
            .try_into()?,
            input_note_nullifiers,
            output_notes: proto::required(details.output_notes, MESSAGE, "output notes")?
                .decode_and_verify()?,
            block_num: proto::required(details.block_num, MESSAGE, "block number")?
                .decode_and_verify()?,
            submission_height: proto::required(
                details.submission_height,
                MESSAGE,
                "submission height",
            )?
            .decode_and_verify()?,
            expiration_block_num: proto::required(
                details.expiration_block_num,
                MESSAGE,
                "expiration block number",
            )?
            .decode_and_verify()?,
            creation_timestamp: details.creation_timestamp,
        })
    }
}

/// Represents the cause of the discarded transaction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiscardCause {
    Expired,
    InputConsumed,
    DiscardedInitialState,
    Stale,
    /// Another transaction reached the same account nonce first, so this one can never commit.
    Superseded,
}

impl DiscardCause {
    pub fn from_string(cause: &str) -> Result<Self, DeserializationError> {
        match cause {
            "Expired" => Ok(DiscardCause::Expired),
            "InputConsumed" => Ok(DiscardCause::InputConsumed),
            "DiscardedInitialState" => Ok(DiscardCause::DiscardedInitialState),
            "Stale" => Ok(DiscardCause::Stale),
            "Superseded" => Ok(DiscardCause::Superseded),
            _ => Err(DeserializationError::InvalidValue(format!("Invalid discard cause: {cause}"))),
        }
    }
}

impl fmt::Display for DiscardCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiscardCause::Expired => write!(f, "Expired"),
            DiscardCause::InputConsumed => write!(f, "InputConsumed"),
            DiscardCause::DiscardedInitialState => write!(f, "DiscardedInitialState"),
            DiscardCause::Stale => write!(f, "Stale"),
            DiscardCause::Superseded => write!(f, "Superseded"),
        }
    }
}

impl Serializable for DiscardCause {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        match self {
            DiscardCause::Expired => target.write_u8(0),
            DiscardCause::InputConsumed => target.write_u8(1),
            DiscardCause::DiscardedInitialState => target.write_u8(2),
            DiscardCause::Stale => target.write_u8(3),
            DiscardCause::Superseded => target.write_u8(4),
        }
    }
}

impl Deserializable for DiscardCause {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        match source.read_u8()? {
            0 => Ok(DiscardCause::Expired),
            1 => Ok(DiscardCause::InputConsumed),
            2 => Ok(DiscardCause::DiscardedInitialState),
            3 => Ok(DiscardCause::Stale),
            4 => Ok(DiscardCause::Superseded),
            _ => Err(DeserializationError::InvalidValue("Invalid discard cause".to_string())),
        }
    }
}

impl From<DiscardCause> for proto::DiscardCause {
    fn from(cause: DiscardCause) -> Self {
        match cause {
            DiscardCause::Expired => proto::DiscardCause::Expired,
            DiscardCause::InputConsumed => proto::DiscardCause::InputConsumed,
            DiscardCause::DiscardedInitialState => proto::DiscardCause::DiscardedInitialState,
            DiscardCause::Stale => proto::DiscardCause::Stale,
            DiscardCause::Superseded => proto::DiscardCause::Superseded,
        }
    }
}

impl TryFrom<i32> for DiscardCause {
    type Error = ProtoDecodeError;

    /// Reads the cause from the enum value that the store keeps. An unknown value and the
    /// unspecified value are both rejected, because the cause decides how the client reports a
    /// transaction that can never commit.
    fn try_from(cause: i32) -> Result<Self, Self::Error> {
        let cause = proto::DiscardCause::try_from(cause).map_err(|_| {
            ProtoDecodeError::InvalidValue(format!("unknown discard cause {cause}"))
        })?;

        match cause {
            proto::DiscardCause::Expired => Ok(DiscardCause::Expired),
            proto::DiscardCause::InputConsumed => Ok(DiscardCause::InputConsumed),
            proto::DiscardCause::DiscardedInitialState => Ok(DiscardCause::DiscardedInitialState),
            proto::DiscardCause::Stale => Ok(DiscardCause::Stale),
            proto::DiscardCause::Superseded => Ok(DiscardCause::Superseded),
            proto::DiscardCause::Unspecified => {
                Err(ProtoDecodeError::InvalidValue("discard cause is unspecified".to_string()))
            },
        }
    }
}

/// Represents the status of a transaction.
#[derive(Debug, Clone, PartialEq)]
pub enum TransactionStatus {
    /// Transaction has been submitted but not yet committed.
    Pending,
    /// Transaction has been committed and included at the specified block number.
    Committed {
        /// Block number at which the transaction was committed.
        block_number: BlockNumber,
        /// Timestamp indicating when the transaction was committed.
        commit_timestamp: u64,
    },
    /// Transaction has been discarded and isn't included in the node.
    Discarded(DiscardCause),
}

pub enum TransactionStatusVariant {
    Pending = 0,
    Committed = 1,
    Discarded = 2,
}

impl TransactionStatus {
    pub const fn variant(&self) -> TransactionStatusVariant {
        match self {
            TransactionStatus::Pending => TransactionStatusVariant::Pending,
            TransactionStatus::Committed { .. } => TransactionStatusVariant::Committed,
            TransactionStatus::Discarded(_) => TransactionStatusVariant::Discarded,
        }
    }
}

impl fmt::Display for TransactionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransactionStatus::Pending => write!(f, "Pending"),
            TransactionStatus::Committed { block_number, .. } => {
                write!(f, "Committed (Block: {block_number})")
            },
            TransactionStatus::Discarded(cause) => write!(f, "Discarded ({cause})"),
        }
    }
}

impl Serializable for TransactionStatus {
    fn write_into<W: ByteWriter>(&self, target: &mut W) {
        match self {
            TransactionStatus::Pending => target.write_u8(self.variant() as u8),
            TransactionStatus::Committed { block_number, commit_timestamp } => {
                target.write_u8(self.variant() as u8);
                block_number.write_into(target);
                commit_timestamp.write_into(target);
            },
            TransactionStatus::Discarded(cause) => {
                target.write_u8(self.variant() as u8);
                cause.write_into(target);
            },
        }
    }
}

impl Deserializable for TransactionStatus {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        match source.read_u8()? {
            variant if variant == TransactionStatusVariant::Pending as u8 => {
                Ok(TransactionStatus::Pending)
            },
            variant if variant == TransactionStatusVariant::Committed as u8 => {
                let block_number = BlockNumber::read_from(source)?;
                let commit_timestamp = source.read_u64()?;
                Ok(TransactionStatus::Committed { block_number, commit_timestamp })
            },
            variant if variant == TransactionStatusVariant::Discarded as u8 => {
                let cause = DiscardCause::read_from(source)?;
                Ok(TransactionStatus::Discarded(cause))
            },
            _ => Err(DeserializationError::InvalidValue("Invalid transaction status".to_string())),
        }
    }
}

impl ProtobufValue for TransactionStatus {
    type Message = proto::TransactionStatus;

    fn to_proto(&self) -> Self::Message {
        use proto::transaction_status::{Committed, Discarded, Pending, Status};

        let status = match self {
            TransactionStatus::Pending => Status::Pending(Pending {}),
            TransactionStatus::Committed { block_number, commit_timestamp } => {
                Status::Committed(Committed {
                    block_number: Some((*block_number).into()),
                    commit_timestamp: *commit_timestamp,
                })
            },
            TransactionStatus::Discarded(cause) => Status::Discarded(Discarded {
                cause: proto::DiscardCause::from(*cause).into(),
            }),
        };

        Self::Message { status: Some(status) }
    }

    fn from_proto(status: Self::Message) -> Result<Self, ProtoDecodeError> {
        use proto::transaction_status::Status;

        const MESSAGE: &str = "transaction status";

        match proto::required(status.status, MESSAGE, "variant")? {
            Status::Pending(_) => Ok(TransactionStatus::Pending),
            Status::Committed(committed) => Ok(TransactionStatus::Committed {
                block_number: proto::required(committed.block_number, MESSAGE, "block number")?
                    .decode_and_verify()?,
                commit_timestamp: committed.commit_timestamp,
            }),
            Status::Discarded(discarded) => {
                Ok(TransactionStatus::Discarded(discarded.cause.try_into()?))
            },
        }
    }
}

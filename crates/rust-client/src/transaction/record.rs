use alloc::vec::Vec;
use core::fmt;

use miden_protocol::Word;
use miden_protocol::account::AccountId;
use miden_protocol::block::BlockNumber;
use miden_protocol::transaction::{RawOutputNotes, TransactionId, TransactionScript};
use miden_tx::utils::serde::DeserializationError;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

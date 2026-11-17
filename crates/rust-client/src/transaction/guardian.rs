use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

use miden_protocol::Word;
use miden_protocol::account::{AccountCodeInterface, AccountId};
#[cfg(feature = "tonic")]
use miden_protocol::account::{StorageMapPatch, StorageSlotPatch};
use miden_protocol::block::BlockNumber;
#[cfg(feature = "tonic")]
use miden_protocol::transaction::TransactionSummary;
#[cfg(feature = "tonic")]
use miden_standards::account::auth::AuthGuardedMultisig;
use miden_standards::account::interface::{
    AccountComponentInterface,
    AccountComponentInterfaceExt,
};
use miden_tx::AuthenticationError;
use thiserror::Error;

use super::{InputNoteAuthentication, RequestAuthArgs, TransactionRequest};

// SUBMISSION MODE
// ================================================================================================

/// Selects who proves and submits the transactions of the guarded accounts of the client.
///
/// A guarded account is an account with the
/// [`AuthGuardedMultisig`](miden_standards::account::auth::AuthGuardedMultisig) auth component. The
/// mode does not apply to other accounts. The client always proves and submits their transactions.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SubmissionMode {
    /// The client proves and submits the transactions.
    #[default]
    SelfExecuted,
    /// Guardian proves and submits the transactions of guarded accounts. The client never proves
    /// them, and checks that Guardian can reproduce a request before it gives the summary that the
    /// cosigners sign.
    GuardianExecutable,
}

// GUARDIAN EXECUTION
// ================================================================================================

/// The state of a Guardian execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardianExecutionState {
    Pending,
    Proving,
    Submitted,
    Committed,
    Failed,
}

/// A Guardian execution of a signed proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardianExecution {
    /// The commitment of the signed transaction summary.
    pub proposal_id: Word,
    pub state: GuardianExecutionState,
}

// GUARDIAN REQUEST ERROR
// ================================================================================================

/// Errors for a transaction request that Guardian cannot reproduce without a store.
#[derive(Debug, Error)]
pub enum GuardianRequestError {
    #[error("the request has input notes that are not pinned")]
    InputNotesNotPinned,
    #[error("the request does not declare explicit multisig auth args")]
    AuthArgsNotExplicit,
    #[error("the request does not declare the bound block {0} in its block numbers")]
    BoundBlockNotDeclared(BlockNumber),
    #[error("the auth args of the request do not bind an approval expiration")]
    ApprovalExpirationMissing,
}

// GUARDIAN ERROR
// ================================================================================================

/// Errors of a request to Guardian.
#[derive(Debug, Error)]
pub enum GuardianError {
    #[error("the client has no Guardian endpoint")]
    NotConfigured,
    #[error("failed to connect to Guardian")]
    Connection(#[source] Box<dyn core::error::Error + Send + Sync>),
    #[error("Guardian refused the request ({code}): {message}")]
    Refused { code: String, message: String },
    #[error("Guardian returned an invalid response: {0}")]
    InvalidResponse(String),
    #[error("the client has no authenticator to sign Guardian requests")]
    MissingAuthenticator,
    #[error("the keystore holds no cosigner key of account {0}")]
    NoCosignerKey(AccountId),
    #[error("the keystore holds more than one cosigner key of account {0}")]
    MultipleCosignerKeys(AccountId),
    #[error("the cosigner key uses a signature scheme that the client cannot send to Guardian")]
    UnsupportedKeyScheme,
    #[error("failed to sign the Guardian request")]
    Signing(#[source] AuthenticationError),
    #[error("the store cannot tell the current time, which a Guardian request needs")]
    MissingClock,
}

// HELPERS
// ================================================================================================

/// Returns `true` if the auth component of the account is `AuthGuardedMultisig`.
pub(crate) fn is_guarded(code_interface: &AccountCodeInterface) -> bool {
    let procedures: Vec<_> = code_interface.procedures().iter().copied().collect();

    AccountComponentInterface::from_procedures(&procedures)
        .contains(&AccountComponentInterface::AuthGuardedMultisig)
}

/// Returns `true` if the transaction changes the Guardian key of the account.
///
/// The first transaction of a new account creates the Guardian key slot, and that is not a change.
#[cfg(feature = "tonic")]
pub(crate) fn changes_guardian(summary: &TransactionSummary) -> bool {
    match summary
        .account_delta()
        .storage()
        .get(AuthGuardedMultisig::guardian_public_key_slot())
    {
        None | Some(StorageSlotPatch::Map(StorageMapPatch::Create { .. })) => false,
        Some(
            StorageSlotPatch::Map(StorageMapPatch::Update { .. } | StorageMapPatch::Remove)
            | StorageSlotPatch::Value(_),
        ) => true,
    }
}

/// Checks that Guardian can reproduce `request` without a store.
///
/// The multisig auth args are `[BLOCK_WORD, SALT, CONVERSION_INFO]`, and `BLOCK_WORD` is
/// `[bound_block, approval_expiration_block, 0, 0]`. Guardian requires both blocks.
pub(crate) fn check_guardian_request(
    request: &TransactionRequest,
) -> Result<(), GuardianRequestError> {
    let has_inferred_notes = request
        .input_notes()
        .iter()
        .any(|note| matches!(note.authentication(), InputNoteAuthentication::Inferred(_)));
    if has_inferred_notes {
        return Err(GuardianRequestError::InputNotesNotPinned);
    }

    let RequestAuthArgs::Explicit(commitment) = request.auth_args() else {
        return Err(GuardianRequestError::AuthArgsNotExplicit);
    };
    let block_word = request
        .advice_map()
        .get(commitment)
        .ok_or(GuardianRequestError::AuthArgsNotExplicit)?;
    let block_at = |index: usize| {
        block_word
            .get(index)
            .and_then(|felt| u32::try_from(felt.as_canonical_u64()).ok())
            .ok_or(GuardianRequestError::AuthArgsNotExplicit)
    };

    let bound_block = BlockNumber::from(block_at(0)?);
    if !request.block_numbers().contains(&bound_block) {
        return Err(GuardianRequestError::BoundBlockNotDeclared(bound_block));
    }
    if block_at(1)? == 0 {
        return Err(GuardianRequestError::ApprovalExpirationMissing);
    }

    Ok(())
}

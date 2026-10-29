#![allow(deprecated)]

use miden_client::ClientError;

fn match_released_client_error(error: ClientError) {
    match error {
        ClientError::AddressAlreadyTracked(..) => (),
        ClientError::AccountAlreadyTracked(..) => (),
        ClientError::AccountError(..) => (),
        ClientError::AccountLocked(..) => (),
        ClientError::AccountCommitmentMismatch(..) => (),
        ClientError::AccountIsPrivate(..) => (),
        ClientError::AccountIsWatched(..) => (),
        ClientError::AccountIsNetworkAccount(..) => (),
        ClientError::AccountAlreadyAllowed(..) => (),
        ClientError::AccountIsNotNew(..) => (),
        ClientError::AccountWatchedMismatch(..) => (),
        ClientError::AccountNotFoundOnChain(..) => (),
        ClientError::AccountNotAllowlisted(..) => (),
        ClientError::AccountNonceTooLow => (),
        ClientError::AssetError(..) => (),
        ClientError::AccountDataNotFound(..) => (),
        ClientError::BatchBuilder(..) => (),
        ClientError::ChainAnchorError(..) => (),
        ClientError::DataStoreError(..) => (),
        ClientError::PartialBlockchainError(..) => (),
        ClientError::ProposedBatchError(..) => (),
        ClientError::ProvenBatchError(..) => (),
        ClientError::DataDeserializationError(..) => (),
        ClientError::MissingConsumedNoteOrder(..) => (),
        ClientError::MissingNoteConsumptionPosition(..) => (),
        ClientError::NoteNotFoundOnChain(..) => (),
        ClientError::InvalidPartialMmrForest => (),
        ClientError::ChainValidationError(..) => (),
        ClientError::AddNewAccountWithoutSeed => (),
        ClientError::MissingOutputRecipients(..) => (),
        ClientError::NoteError(..) => (),
        ClientError::NoteCheckerError(..) => (),
        ClientError::NoteImportError(..) => (),
        ClientError::NoteRecordConversionError(..) => (),
        ClientError::NoteTransportError(..) => (),
        ClientError::NoConsumableNoteForAccount(..) => (),
        ClientError::RpcError(..) => (),
        ClientError::RecencyConditionError(..) => (),
        ClientError::NoteScreenerError(..) => (),
        ClientError::StoreError(..) => (),
        ClientError::TransactionExecutorError(..) => (),
        ClientError::TransactionProvingError(..) => (),
        ClientError::MismatchedProvenTransaction { .. } => (),
        ClientError::TransactionRequestError(..) => (),
        ClientError::ClientInitializationError(..) => (),
        ClientError::AccountRecordNotFull(..) => (),
        ClientError::AccountRecordNotPartial(..) => (),
        ClientError::NtxScriptRegistrationFailed { .. } => (),
        ClientError::ApplyTransactionAfterSubmitFailed { .. } => (),
        ClientError::SubmissionOutcomeUnknown { .. } => (),
        ClientError::Observer(..) => (),
        ClientError::UnscreenedNoteBlocks => (),
        ClientError::AccountTagLimitExceeded(..) => (),
    }
}

#[test]
fn released_client_error_remains_exhaustively_matchable() {
    match_released_client_error(ClientError::AccountNonceTooLow);
}

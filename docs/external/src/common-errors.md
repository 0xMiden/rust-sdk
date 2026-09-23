## Troubleshooting and transaction lifecycle

This guide helps you troubleshoot common issues and understand the end-to-end lifecycle of transactions and notes in the Miden client.

### Actionable error hints

#### `ClientError::MissingOutputRecipients`
- Cause: The MASM program emitted an output note whose recipient was not listed in `TransactionRequestBuilder::expected_output_recipients(...)`.
- Fix: Reconcile the MASM recipient data with the Rust note structs and update the expected recipients so that the expected recipients are part of the transaction outputs.

#### `TransactionRequestError::MissingAuthenticatedInputNote`
- Cause: A note ID included in `TransactionRequestBuilder::authenticated_input_notes(...)` did not have a corresponding `InputNoteRecord` in the store, or it was not found to contain authentication data.
- Fix: Import or sync the note, so its record and inclusion proof are present before building and executing the request.

#### `TransactionRequestError::NoInputNotesNorAccountChange`
- Cause: The transaction neither consumes input notes nor mutates tracked account state.
- Fix: Add at least one authenticated/unauthenticated input note or include an explicit account state update in the request.

#### `TransactionRequestError::StorageSlotNotFound`
- Cause: The request referenced an account storage slot that does not exist, often because the ABI layout is incorrectly addressed (auth component is always the first component in the account component list).
- Fix: Verify the account ABI and component ordering, then adjust the slot index used in the transaction.

#### `TransactionExecutorError::ForeignAccountNotAnchoredInReference`
- Cause: The foreign account proof was generated against a different block than the request’s reference block.
- Fix: Re-fetch the foreign account proof anchored at the correct reference block and retry.

#### `TransactionExecutorError::TransactionProgramExecutionFailed`
- Cause: The MASM kernel failed during execution (e.g., failed assertion or constraint violation).
- Fix: Re-run with the debug mode, capture VM diagnostics and inspect the source manager output to understand why execution failed.

#### `ClientError::StoreError(AccountCommitmentAlreadyExists(...))`
- Cause: The final account commitment already exists locally, usually because the transaction was applied previously.
- Fix: Sync to confirm the transaction status and avoid resubmitting it; if you need a clean slate for development, reset the store.

#### `ClientError::NoteNotFoundOnChain(Note ID)`/`RpcError::NoteNotFound(Note ID)`
- Cause: The note has not been found on chain, or the input ID is incorrect.
- Fix: Verify the note ID, ensure it has been committed, and run sync the client before retrying.

#### `ClientError::AccountNotAllowlisted(AccountId)`
- Cause: The network enforces an account allowlist, and the submitted transaction or batch would create an account that is not registered on it. Only account creation is gated; network accounts are exempt.
- Fix: Register the account with `Client::register_account` and the invitation code from the network operator before its first transaction. `Client::is_account_allowed` answers whether the network accepts the account.

#### `RpcError::RequestError` with `EndpointError::RegisterAccount(...)`
- Cause: The node rejected the registration: `InvitationNotFound` for an unknown code, `AlreadyRegistered` when the code is bound to a different account or the account is already registered, and `InvalidRequest` for an empty code or an unreadable account ID.
- Fix: Send the code exactly as received. An account that is already registered needs no action. Sync to receive the funding note the network may have paid it.

#### `ClientError::AccountAlreadyAllowed(AccountId)`
- Cause: `Client::register_account` found that the node already allows the account, because it is registered or because the network does not enforce an allowlist, and did not send the invitation code.
- Fix: No registration is needed. Keep the code for a different account.

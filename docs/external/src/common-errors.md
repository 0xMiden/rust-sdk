## Troubleshooting and transaction lifecycle

This guide helps you troubleshoot common issues and understand the end-to-end lifecycle of transactions and notes in the Miden client.

### Actionable error hints

#### `ClientError::MissingOutputRecipients`
- Cause: The transaction did not create an output note for one or more recipients that the request expects. The request expects the recipients set with `TransactionRequestBuilder::expected_output_recipients(...)` and the recipients of the notes set with `own_output_notes(...)`. An output note that the request does not expect does not cause this error.
- Fix: Reconcile the MASM recipient data with the Rust note structs and update the expected recipients so that the expected recipients are part of the transaction outputs.

#### Empty transaction (`executed transaction neither changed the account state, nor consumed any notes`)
- Cause: The transaction did not consume a note and did not change the account state. The transaction kernel rejects such a transaction with this assertion, and the client returns `TransactionExecutorError::TransactionProgramExecutionFailed`.
- Fix: Add an input note or an account state change to the request.

#### `TransactionRequestError::ForeignAccountNotAtReferenceBlock`
- Cause: The request carries prefetched foreign account inputs (`ForeignAccount::Prefetched`) that do not open against the account tree of the transaction's reference block. This occurs, for example, when the client syncs between fetching the inputs and executing the transaction.
- Fix: Fetch the foreign account inputs again at the reference block and retry. Do not sync between fetching and executing. As an alternative, declare the account with `ForeignAccount::public(...)` or `ForeignAccount::private(...)`, so that the client fetches the inputs itself.

#### `TransactionExecutorError::TransactionProgramExecutionFailed`
- Cause: The MASM kernel failed during execution (e.g., failed assertion or constraint violation).
- Fix: Build the CLI with the `dap` feature and re-run `exec` or `consume-notes` with `--start-debug-adapter <ADDR>`. Step through the transaction in a DAP client to find the failing instruction. See the [debugging guide](./rust-client/debugging.md).

#### `ClientError::NoteNotFoundOnChain(Note ID)`/`RpcError::NoteNotFound(Note ID)`
- Cause: The note has not been found on chain, or the input ID is incorrect.
- Fix: Verify the note ID, ensure it has been committed, and sync the client before retrying.

#### `RpcError::RequestError` with a `FutureBlock` endpoint error
- Cause: A sync request asked for a block that is ahead of the chain tip of the node that served it. This usually happens when a follower node is behind the network.
- Fix: Retry the request after a short delay. The node serves the block when it catches up.

#### `ClientError::AccountNotAllowlisted(AccountId)`
- Cause: The network enforces an account allowlist, and the submitted transaction or batch would create an account that is not registered on it. Only account creation is gated; network accounts are exempt.
- Fix: Register the account with `Client::register_account` and the invitation code from the network operator before its first transaction. `Client::is_account_allowed` answers whether the network accepts the account.

#### `RpcError::RequestError` with `EndpointError::RegisterAccount(...)`
- Cause: The node rejected the registration: `InvitationNotFound` for an unknown code, `AlreadyRegistered` when the code is bound to a different account or the account is already registered, and `InvalidRequest` for an empty code or an unreadable account ID.
- Fix: Send the code exactly as received. An account that is already registered needs no action. Sync to receive the funding note the network may have paid it.

#### `ClientError::AccountAlreadyAllowed(AccountId)`
- Cause: `Client::register_account` found that the node already allows the account, because it is registered or because the network does not enforce an allowlist, and did not send the invitation code.
- Fix: No registration is needed. Keep the code for a different account.

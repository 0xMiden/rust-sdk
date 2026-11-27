//! A wrapper client for one guarded multisig account that orchestrates the interaction between
//! `miden-client` and Guardian.
//!
//! [`GuardedClient`] owns a [`Client`] and keeps its store up to date for the account. The account
//! transactions follow one of two paths:
//!
//! - Self-executed: the client proves and submits a request that carries every signature.
//! - Guardian-executed: Guardian executes, proves and submits a proposal that the cosigners signed.
//!   The client never proves it. After Guardian commits the transaction, [`GuardedClient::sync`]
//!   updates the account and the output notes.

use std::time::Duration;

use miden_client::store::{InputNoteRecord, NoteFilter, OutputNoteRecord};
use miden_client::sync::SyncSummary;
use miden_client::transaction::{TransactionId, TransactionRequest, TransactionSummary};
use miden_client::{Client, ClientError};
use miden_protocol::Word;
use miden_protocol::account::auth::PublicKeyCommitment;
use miden_protocol::account::{
    Account,
    AccountId,
    PartialAccount,
    StorageMapKey,
    StorageMapPatch,
    StorageSlotPatch,
};
use miden_protocol::block::BlockNumber;
use miden_standards::account::auth::AuthGuardedMultisig;
use miden_standards::account::interface::{
    AccountComponentInterface,
    AccountComponentInterfaceExt,
};
use miden_tx::auth::TransactionAuthenticator;
use tokio::time::Instant;

mod errors;
mod grpc;

pub use errors::GuardedClientError;
use grpc::GrpcGuardianClient;

#[allow(clippy::all, clippy::pedantic, dead_code)]
mod generated {
    tonic::include_proto!("guardian");
}

/// The maximum number of blocks that a request for Guardian can declare.
const MAX_DECLARED_BLOCKS: usize = 16;

const INITIAL_POLL_DELAY: Duration = Duration::from_secs(1);
const MAX_POLL_DELAY: Duration = Duration::from_secs(10);

// GUARDIAN EXECUTION
// ================================================================================================

/// The state of a Guardian execution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardianExecutionState {
    Pending,
    Proving,
    Submitted,
    Committed,
    Failed(ExecutionFailure),
}

/// Why a Guardian execution failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionFailure {
    /// A `GUARDIAN_EXECUTION_*` code.
    pub code: String,
    pub message: String,
    /// The details that Guardian attaches to some codes, as JSON.
    pub meta_json: Option<String>,
}

/// A Guardian execution of a signed proposal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GuardianExecution {
    /// The commitment of the signed transaction summary.
    pub proposal_id: Word,
    pub state: GuardianExecutionState,
}

// GUARDED CLIENT
// ================================================================================================

/// A client for one guarded multisig account.
///
/// It signs the Guardian requests with the cosigner key that the client authenticator holds.
pub struct GuardedClient<AUTH> {
    client: Client<AUTH>,
    account_id: AccountId,
    guardian: GrpcGuardianClient<AUTH>,
}

impl<AUTH> GuardedClient<AUTH>
where
    AUTH: TransactionAuthenticator + Sync + 'static,
{
    /// Creates a client for `account_id`, guarded by the Guardian at `guardian_endpoint`.
    ///
    /// # Errors
    ///
    /// - Returns [`GuardedClientError::AccountNotTracked`] if `client` does not track the account.
    /// - Returns [`GuardedClientError::AccountNotGuarded`] if the auth component of the account is
    ///   not `AuthGuardedMultisig`.
    /// - Returns [`GuardedClientError::MissingAuthenticator`] if `client` has no authenticator.
    /// - Returns [`GuardedClientError::CosignerKeyNotHeld`] if the authenticator does not hold
    ///   `cosigner`.
    pub async fn new(
        client: Client<AUTH>,
        account_id: AccountId,
        guardian_endpoint: String,
        cosigner: PublicKeyCommitment,
    ) -> Result<Self, GuardedClientError> {
        let account = client
            .get_account(account_id)
            .await?
            .ok_or(GuardedClientError::AccountNotTracked(account_id))?;
        if !is_guarded(&account) {
            return Err(GuardedClientError::AccountNotGuarded(account_id));
        }

        let authenticator = client
            .authenticator()
            .cloned()
            .ok_or(GuardedClientError::MissingAuthenticator)?;
        if authenticator.get_public_key(cosigner).await.is_none() {
            return Err(GuardedClientError::CosignerKeyNotHeld);
        }

        let guardian =
            GrpcGuardianClient::new(guardian_endpoint, account_id, cosigner, authenticator);
        Ok(Self { client, account_id, guardian })
    }

    /// Creates a client for `account_id` with the account state that Guardian holds. `client` must
    /// not track the account. See [`Self::new`].
    ///
    /// # Errors
    ///
    /// - Returns [`GuardedClientError::Refused`] if Guardian refuses the state request.
    /// - Returns the errors of [`Self::new`].
    pub async fn import(
        mut client: Client<AUTH>,
        account_id: AccountId,
        guardian_endpoint: String,
        cosigner: PublicKeyCommitment,
    ) -> Result<Self, GuardedClientError> {
        let authenticator = client
            .authenticator()
            .cloned()
            .ok_or(GuardedClientError::MissingAuthenticator)?;
        let guardian =
            GrpcGuardianClient::new(guardian_endpoint.clone(), account_id, cosigner, authenticator);
        let account = guardian.get_state().await?;
        client.add_account(&account, false).await?;

        Self::new(client, account_id, guardian_endpoint, cosigner).await
    }

    /// Returns the commitment of the key that the Guardian at `guardian_endpoint` signs with. A
    /// guarded account stores it, so the account is built with it.
    pub async fn guardian_key(
        guardian_endpoint: String,
    ) -> Result<PublicKeyCommitment, GuardedClientError> {
        grpc::guardian_key(guardian_endpoint).await
    }

    // REGISTRATION
    // --------------------------------------------------------------------------------------------

    /// Registers the account in Guardian with the account state in the store as its initial state.
    ///
    /// Guardian accepts the registration only if the cosigners match the approvers that the account
    /// stores, so the client reads them from the account.
    pub async fn register(&self) -> Result<(), GuardedClientError> {
        let account = self.account().await?;
        self.guardian.configure(&account, &cosigner_commitments(&account)).await
    }

    // TRANSACTIONS
    // --------------------------------------------------------------------------------------------

    /// Submits a Guardian proposal for `request`, signed with the cosigner key. Returns the summary
    /// of the transaction. Its commitment is the ID of the proposal.
    ///
    /// Guardian executes the proposal again from `request`, so the request must be reproducible
    /// (see [`TransactionRequest::check_reproducible`]) and must declare the block that the summary
    /// binds. The client sends `request` without changes.
    ///
    /// # Errors
    ///
    /// - Returns [`GuardedClientError::Client`] if `request` is not reproducible or its execution
    ///   fails.
    /// - Returns [`GuardedClientError::ApprovalExpirationMissing`] if the multisig auth args set no
    ///   approval expiration.
    /// - Returns [`GuardedClientError::BoundBlockNotDeclared`] if `request` does not declare the
    ///   block that the summary binds.
    /// - Returns [`GuardedClientError::TooManyDeclaredBlocks`] if `request` declares more blocks
    ///   than Guardian accepts.
    /// - Returns [`GuardedClientError::UnsupportedKeyScheme`] if the cosigner key is not a Falcon
    ///   key.
    /// - Returns [`GuardedClientError::Refused`] if Guardian refuses the proposal.
    pub async fn propose(
        &self,
        request: TransactionRequest,
    ) -> Result<TransactionSummary, GuardedClientError> {
        request.check_reproducible().map_err(ClientError::from)?;
        let summary = self.client.execute_for_summary(self.account_id, request.clone()).await?;

        // The multisig auth procedure puts the approval expiration block in the first user param.
        // The value 0 means that the approval does not expire.
        if summary.user_params().as_elements()[0].as_canonical_u64() == 0 {
            return Err(GuardedClientError::ApprovalExpirationMissing);
        }
        if !request.block_numbers().contains(&summary.block_number()) {
            return Err(GuardedClientError::BoundBlockNotDeclared(summary.block_number()));
        }
        let declared_blocks = request.block_numbers().len();
        if declared_blocks > MAX_DECLARED_BLOCKS {
            return Err(GuardedClientError::TooManyDeclaredBlocks(declared_blocks));
        }

        // A proposal names the nonce that the account has after the transaction.
        let nonce = self.account().await?.nonce().as_canonical_u64() + 1;
        self.guardian.push_proposal(nonce, &summary, &request).await?;

        Ok(summary)
    }

    /// Returns the summary of the proposal with ID `proposal_id`.
    pub async fn proposal_summary(
        &self,
        proposal_id: Word,
    ) -> Result<TransactionSummary, GuardedClientError> {
        self.guardian.proposal_summary(proposal_id).await
    }

    /// Proves and submits `request` with the client. The request must carry every signature that
    /// the auth procedure of the account needs.
    pub async fn submit_transaction(
        &mut self,
        request: TransactionRequest,
    ) -> Result<TransactionId, GuardedClientError> {
        Ok(self.client.submit_new_transaction(self.account_id, request).await?)
    }

    /// Asks Guardian to execute, prove and submit the proposal with `summary`, after the cosigners
    /// signed it.
    ///
    /// The client stores the output notes of `summary` as expected notes, so that sync commits them
    /// when Guardian commits the transaction.
    ///
    /// # Errors
    ///
    /// - Returns [`GuardedClientError::SummaryAccountMismatch`] if `summary` is for another
    ///   account.
    /// - Returns [`GuardedClientError::GuardianSwitchExecutesLocally`] if the transaction changes
    ///   the Guardian of the account. Use [`Self::submit_transaction`] for it.
    /// - Returns [`GuardedClientError::Refused`] if Guardian refuses the execution.
    pub async fn request_guardian_execution(
        &mut self,
        summary: &TransactionSummary,
    ) -> Result<GuardianExecution, GuardedClientError> {
        let summary_account = summary.account_delta().id();
        if summary_account != self.account_id {
            return Err(GuardedClientError::SummaryAccountMismatch(summary_account));
        }
        if changes_guardian(summary) {
            return Err(GuardedClientError::GuardianSwitchExecutesLocally);
        }

        let execution = self.guardian.execute(summary.to_commitment()).await?;
        if !matches!(execution.state, GuardianExecutionState::Failed(_)) {
            self.client
                .track_expected_output_notes(summary.output_notes().iter().cloned())
                .await?;
        }

        Ok(execution)
    }

    /// Returns the latest Guardian execution of the proposal with ID `proposal_id`.
    pub async fn execution_status(
        &self,
        proposal_id: Word,
    ) -> Result<GuardianExecution, GuardedClientError> {
        self.guardian.execution_status(proposal_id).await
    }

    /// Waits until the Guardian execution of the proposal with ID `proposal_id` is committed.
    ///
    /// # Errors
    ///
    /// - Returns [`GuardedClientError::ExecutionFailed`] if the execution fails.
    /// - Returns [`GuardedClientError::WaitTimedOut`] if the execution does not end within
    ///   `timeout`.
    /// - Returns [`GuardedClientError::Refused`] if Guardian refuses a status request.
    pub async fn wait_for_execution(
        &self,
        proposal_id: Word,
        timeout: Duration,
    ) -> Result<GuardianExecution, GuardedClientError> {
        let deadline = Instant::now() + timeout;
        let mut delay = INITIAL_POLL_DELAY;

        loop {
            let execution = self.guardian.execution_status(proposal_id).await?;
            if execution.state == GuardianExecutionState::Committed {
                return Ok(execution);
            }
            if let GuardianExecutionState::Failed(failure) = execution.state {
                return Err(GuardedClientError::ExecutionFailed(failure));
            }

            if Instant::now() + delay > deadline {
                return Err(GuardedClientError::WaitTimedOut);
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_POLL_DELAY);
        }
    }

    // SYNC
    // --------------------------------------------------------------------------------------------

    /// Syncs the client with the network, and then updates a locked account with the state that
    /// Guardian holds.
    ///
    /// Sync locks a private account when another party commits a transaction of the account, for
    /// example Guardian. The client accepts the Guardian state only if its commitment is the
    /// on-chain commitment at the sync height.
    ///
    /// # Errors
    ///
    /// - Returns [`GuardedClientError::Client`] with
    ///   [`miden_client::ClientError::AccountCommitmentMismatch`] if the Guardian state is not the
    ///   on-chain state at the sync height. The account stays locked, and a later sync tries again.
    pub async fn sync(&mut self) -> Result<SyncSummary, GuardedClientError> {
        let summary = self.client.sync_state().await?;

        let (_, status) = self
            .client
            .get_account_header(self.account_id)
            .await?
            .ok_or(GuardedClientError::AccountNotTracked(self.account_id))?;
        if status.is_locked() {
            // `add_account` checks the state against the chain only for a tracked locked account.
            let account = self.guardian.get_state().await?;
            self.client.add_account(&account, true).await?;
        }

        Ok(summary)
    }

    // READS
    // --------------------------------------------------------------------------------------------

    /// Returns the ID of the account.
    pub fn account_id(&self) -> AccountId {
        self.account_id
    }

    /// Returns the account state in the store.
    pub async fn account(&self) -> Result<Account, GuardedClientError> {
        self.client
            .get_account(self.account_id)
            .await?
            .ok_or(GuardedClientError::AccountNotTracked(self.account_id))
    }

    /// Returns the input notes in the store. See [`Client::get_input_notes`].
    pub async fn get_input_notes(
        &self,
        filter: NoteFilter,
    ) -> Result<Vec<InputNoteRecord>, GuardedClientError> {
        Ok(self.client.get_input_notes(filter).await?)
    }

    /// Returns the output notes in the store. See [`Client::get_output_notes`].
    pub async fn get_output_notes(
        &self,
        filter: NoteFilter,
    ) -> Result<Vec<OutputNoteRecord>, GuardedClientError> {
        Ok(self.client.get_output_notes(filter).await?)
    }

    /// Returns the block number of the last sync.
    pub async fn sync_height(&self) -> Result<BlockNumber, GuardedClientError> {
        Ok(self.client.get_sync_height().await?)
    }
}

// HELPERS
// ================================================================================================

/// Returns `true` if the auth component of the account is `AuthGuardedMultisig`.
fn is_guarded(account: &Account) -> bool {
    let code_interface = PartialAccount::from(account).code_interface();
    let procedures: Vec<_> = code_interface.procedures().iter().copied().collect();

    AccountComponentInterface::from_procedures(&procedures)
        .contains(&AccountComponentInterface::AuthGuardedMultisig)
}

/// Returns the key commitments of the approvers of the account, in the order of the storage map.
/// The map keys are the indexes `[0, 0, 0, 0]`, `[1, 0, 0, 0]`, and so on.
fn cosigner_commitments(account: &Account) -> Vec<PublicKeyCommitment> {
    let slot = AuthGuardedMultisig::approver_public_keys_slot();
    (0u32..)
        .map(|index| {
            account
                .storage()
                .get_map_item(slot, StorageMapKey::new(Word::from([index, 0, 0, 0])))
        })
        .map_while(|item| item.ok().filter(|commitment| *commitment != Word::empty()))
        .map(PublicKeyCommitment::from)
        .collect()
}

/// Returns `true` if the transaction changes the Guardian key of the account.
///
/// The first transaction of a new account creates the Guardian key slot, and that is not a change.
fn changes_guardian(summary: &TransactionSummary) -> bool {
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

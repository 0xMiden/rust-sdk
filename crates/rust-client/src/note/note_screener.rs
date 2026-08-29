use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use async_trait::async_trait;
use miden_protocol::Word;
use miden_protocol::account::{AccountCode, AccountId};
use miden_protocol::block::BlockNumber;
use miden_protocol::note::{Note, NoteId};
use miden_standards::account::auth::commit_fee_conversion_info;
use miden_standards::note::NoteConsumptionStatus;
use miden_tx::{
    NoteCheckerError,
    NoteConsumptionChecker,
    NoteConsumptionInfo,
    TransactionExecutor,
};
use thiserror::Error;

use crate::ClientError;
use crate::rpc::NodeRpcClient;
use crate::rpc::domain::note::CommittedNote;
use crate::store::data_store::ClientDataStore;
use crate::store::{InputNoteRecord, NoteFilter, Store, StoreError};
use crate::sync::{NoteUpdateAction, OnNoteReceived};
use crate::transaction::{
    AdviceMap,
    InputNote,
    NATIVE_FEE_CONVERSION_SALT,
    TransactionArgs,
    TransactionRequestError,
    native_fee_conversion_info,
};

/// Represents the consumability of a note by a specific account.
///
/// The tuple contains the account ID that may consume the note and the moment it will become
/// relevant.
pub type NoteConsumability = (AccountId, NoteConsumptionStatus);

/// Returns `true` if the consumption status indicates that the note may be consumable by the
/// account. A note is considered relevant unless it is permanently unconsumable (either due to
/// a fundamental incompatibility or unconsumable conditions).
fn is_relevant(consumption_status: &NoteConsumptionStatus) -> bool {
    !matches!(
        consumption_status,
        NoteConsumptionStatus::NeverConsumable(_) | NoteConsumptionStatus::UnconsumableConditions
    )
}

/// Provides functionality for testing whether a note is relevant to the client or not.
///
/// Here, relevance is based on whether the note is able to be consumed by an account that is
/// tracked in the provided `store`. This can be derived in a number of ways, such as looking
/// at the combination of script root and note inputs. For example, a P2ID note is relevant
/// for a specific account ID if this ID is its first note input.
#[derive(Clone)]
pub struct NoteScreener {
    /// A reference to the client's store, used to fetch necessary data to check consumability.
    store: Arc<dyn Store>,
    /// Optional transaction arguments to use when checking consumability.
    tx_args: Option<TransactionArgs>,
    /// RPC client used for lazy-loading foreign account data during note screening.
    rpc_api: Arc<dyn NodeRpcClient>,
}

impl NoteScreener {
    pub fn new(store: Arc<dyn Store>, rpc_api: Arc<dyn NodeRpcClient>) -> Self {
        Self { store, tx_args: None, rpc_api }
    }

    /// Sets the transaction arguments to use when checking note consumability.
    /// If not set, a default `TransactionArgs` with an empty advice map is used.
    #[must_use]
    pub fn with_transaction_args(mut self, tx_args: TransactionArgs) -> Self {
        self.tx_args = Some(tx_args);
        self
    }

    fn tx_args(&self) -> TransactionArgs {
        self.tx_args
            .clone()
            .unwrap_or_else(|| TransactionArgs::new(AdviceMap::default()))
    }

    /// Checks whether the provided note could be consumed by any of the accounts tracked by
    /// this screener. Convenience wrapper around [`Self::get_batch_consumability`] for a single
    /// note.
    ///
    /// Returns the [`NoteConsumptionStatus`] for each account that could consume the note.
    pub async fn get_consumability(
        &self,
        note: &Note,
    ) -> Result<Vec<NoteConsumability>, NoteScreenerError> {
        Ok(self
            .get_batch_consumability(core::slice::from_ref(note))
            .await?
            .remove(&note.id())
            .unwrap_or_default())
    }

    /// Checks whether the provided notes could be consumed by any of the accounts tracked by
    /// this screener, by executing a transaction for each note-account pair.
    ///
    /// Returns a map from [`NoteId`] to a list of `(AccountId, NoteConsumptionStatus)` pairs.
    /// Notes that are permanently unconsumable by all accounts are not included in the result.
    pub async fn get_batch_consumability(
        &self,
        notes: &[Note],
    ) -> Result<BTreeMap<NoteId, Vec<NoteConsumability>>, NoteScreenerError> {
        let account_ids = self.store.get_account_ids().await?;
        self.screen_notes(notes, account_ids).await
    }

    /// Checks whether the provided notes could be consumed by `account_id`, by executing a
    /// transaction for each note. Unlike [`Self::get_batch_consumability`], only `account_id` is
    /// screened instead of every account tracked by this screener.
    ///
    /// Returns a map from [`NoteId`] to a single-element list holding `account_id` and its
    /// [`NoteConsumptionStatus`]. Notes that `account_id` cannot consume are not included in the
    /// result.
    pub async fn get_batch_consumability_for_account(
        &self,
        account_id: AccountId,
        notes: &[Note],
    ) -> Result<BTreeMap<NoteId, Vec<NoteConsumability>>, NoteScreenerError> {
        self.screen_notes(notes, vec![account_id]).await
    }

    /// Screens `notes` against `account_ids`, executing a transaction for each note-account pair
    /// and collecting the accounts that could consume each note.
    async fn screen_notes(
        &self,
        notes: &[Note],
        account_ids: Vec<AccountId>,
    ) -> Result<BTreeMap<NoteId, Vec<NoteConsumability>>, NoteScreenerError> {
        if notes.is_empty() || account_ids.is_empty() {
            return Ok(BTreeMap::new());
        }

        let block_ref = self.store.get_sync_height().await?;
        let mut relevant_notes: BTreeMap<NoteId, Vec<NoteConsumability>> = BTreeMap::new();
        let tx_args = self.tx_args();

        let data_store = ClientDataStore::new(self.store.clone(), self.rpc_api.clone())
            .with_execution_input_cache();
        // Don't attach the real authenticator for consumability checks. The
        // NoteConsumptionChecker gracefully handles a missing authenticator by
        // returning `ConsumableWithAuthorization` instead of calling
        // `get_signature()`. Attaching the real authenticator here causes the
        // external signer (e.g. wallet extension) to be invoked during
        // sync_state, producing unwanted confirmation popups on every sync.
        let transaction_executor: TransactionExecutor<'_, '_, _, ()> =
            TransactionExecutor::new(&data_store);
        let consumption_checker = NoteConsumptionChecker::new(&transaction_executor);

        for account_id in account_ids {
            let account_code = self.get_account_code(account_id).await?;
            data_store.mast_store().load_account_code(&account_code);

            let account_tx_args = self
                .with_native_fee_conversion_info(
                    tx_args.clone(),
                    account_id,
                    &account_code,
                    block_ref,
                )
                .await?;

            for note in notes {
                let consumption_status = consumption_checker
                    .can_consume(
                        account_id,
                        block_ref,
                        InputNote::unauthenticated(note.clone()),
                        account_tx_args.clone(),
                    )
                    .await?;

                if is_relevant(&consumption_status) {
                    relevant_notes
                        .entry(note.id())
                        .or_default()
                        .push((account_id, consumption_status));
                }
            }
        }

        Ok(relevant_notes)
    }

    /// Checks whether the provided notes could be consumed by a specific account by attempting
    /// to execute them together in a transaction. Notes that fail are progressively removed
    /// until a maximal set of successfully consumable notes is found.
    ///
    /// Returns a [`NoteConsumptionInfo`] splitting notes into those that succeeded and those
    /// that failed.
    pub async fn check_notes_consumability(
        &self,
        account_id: AccountId,
        notes: Vec<Note>,
    ) -> Result<NoteConsumptionInfo, NoteScreenerError> {
        let block_ref = self.store.get_sync_height().await?;
        let account_code = self.get_account_code(account_id).await?;
        let tx_args = self
            .with_native_fee_conversion_info(self.tx_args(), account_id, &account_code, block_ref)
            .await?;

        let data_store = ClientDataStore::new(self.store.clone(), self.rpc_api.clone())
            .with_execution_input_cache();
        let transaction_executor: TransactionExecutor<'_, '_, _, ()> =
            TransactionExecutor::new(&data_store);

        let consumption_checker = NoteConsumptionChecker::new(&transaction_executor);

        data_store.mast_store().load_account_code(&account_code);
        let note_consumption_info = consumption_checker
            .check_notes_consumability(account_id, block_ref, notes, tx_args)
            .await?;

        Ok(note_consumption_info)
    }

    /// Returns `tx_args` carrying the auth arg the account needs to settle its fee, when the
    /// chain at `block_ref` charges one.
    ///
    /// Screening executes the full transaction kernel, auth procedure included, so on a
    /// fee-charging chain `fee::pay_fee` aborts when a non-zero fee meets auth args with no
    /// conversion info. [`NoteConsumptionChecker`] reports that abort as
    /// [`NoteConsumptionStatus::UnconsumableConditions`], which would drop the note from the sync.
    /// Standard notes are unaffected — they are answered without executing anything — so this
    /// matters for notes carrying a custom script.
    ///
    /// The info comes from [`native_fee_conversion_info`], the same source the execution path
    /// uses, so a note is measured against the fee the transaction it stands in for would pay.
    async fn with_native_fee_conversion_info(
        &self,
        tx_args: TransactionArgs,
        account_id: AccountId,
        account_code: &AccountCode,
        block_ref: BlockNumber,
    ) -> Result<TransactionArgs, NoteScreenerError> {
        // Auth args the caller set are the caller's business, as on the execution path.
        if tx_args.auth_args() != Word::empty() {
            return Ok(tx_args);
        }

        // A missing header is left to the trial execution, which reads the same one through the
        // data store and reports its absence with a more specific error than this could.
        let Some((header, _)) = self.store.get_block_header_by_num(block_ref).await? else {
            return Ok(tx_args);
        };

        let Some(conversion_info) = native_fee_conversion_info(
            &account_code.interface(account_id),
            header.fee_parameters(),
        ) else {
            return Ok(tx_args);
        };

        let (auth_arg, preimage) =
            commit_fee_conversion_info(conversion_info, NATIVE_FEE_CONVERSION_SALT);
        let mut tx_args = tx_args.with_auth_args(auth_arg);
        tx_args.extend_advice_map([(auth_arg, preimage)]);

        Ok(tx_args)
    }

    async fn get_account_code(
        &self,
        account_id: AccountId,
    ) -> Result<AccountCode, NoteScreenerError> {
        self.store
            .get_account_code(account_id)
            .await?
            .ok_or(NoteScreenerError::AccountDataNotFound(account_id))
    }
}

// DEFAULT CALLBACK IMPLEMENTATIONS
// ================================================================================================

#[async_trait(?Send)]
impl OnNoteReceived for NoteScreener {
    /// Default implementation of the [`OnNoteReceived`] callback. It queries the store for the
    /// committed note to check if it's relevant. If the note wasn't being tracked but it came in
    /// the sync response it may be a new public note, in that case we use the [`NoteScreener`]
    /// to check its relevance.
    async fn on_note_received(
        &self,
        committed_note: CommittedNote,
        public_note: Option<InputNoteRecord>,
    ) -> Result<NoteUpdateAction, ClientError> {
        let note_id = *committed_note.note_id();

        let mut input_note_present =
            !self.store.get_input_notes(NoteFilter::Unique(note_id)).await?.is_empty();

        // Notes imported without metadata (e.g. via `NoteFile::NoteDetails`) have a NULL `note_id`
        // and so can't be matched by id. Recognize them by reconstructing their id from the
        // committed metadata: `NoteId::new(details_commitment, metadata)`.
        // TODO: revisit
        if !input_note_present {
            input_note_present = self
                .store
                .get_input_notes(NoteFilter::Expected)
                .await?
                .iter()
                .filter(|note| note.metadata().is_none())
                .any(|note| {
                    NoteId::new(note.details_commitment(), committed_note.metadata()) == note_id
                });
        }

        let output_note_present =
            !self.store.get_output_notes(NoteFilter::Unique(note_id)).await?.is_empty();

        if input_note_present || output_note_present {
            // The note is being tracked by the client so it is relevant
            return Ok(NoteUpdateAction::Commit(committed_note));
        }

        match public_note {
            Some(public_note) => {
                // If tracked by the user, keep note regardless of inputs and extra checks
                if let Some(metadata) = public_note.metadata()
                    && self.store.get_unique_note_tags().await?.contains(&metadata.tag())
                {
                    return Ok(NoteUpdateAction::Insert(public_note));
                }

                // The note is not being tracked by the client and is public so we can screen it
                let new_note_relevance = self
                    .get_consumability(
                        &public_note
                            .clone()
                            .try_into()
                            .map_err(ClientError::NoteRecordConversionError)?,
                    )
                    .await?;
                let is_relevant = !new_note_relevance.is_empty();
                if is_relevant {
                    Ok(NoteUpdateAction::Insert(public_note))
                } else {
                    Ok(NoteUpdateAction::Discard)
                }
            },
            None => {
                // The note is not being tracked by the client and is private so we can't determine
                // if it is relevant
                Ok(NoteUpdateAction::Discard)
            },
        }
    }
}

// NOTE SCREENER ERRORS
// ================================================================================================

/// Error when screening notes to check relevance to a client.
#[derive(Debug, Error)]
pub enum NoteScreenerError {
    #[error("account {0} data not found in the store")]
    AccountDataNotFound(AccountId),
    #[error("failed to fetch data from the store")]
    StoreError(#[from] StoreError),
    #[error("note consumption check failed")]
    NoteCheckerError(#[from] NoteCheckerError),
    #[error("failed to build transaction request")]
    TransactionRequestError(#[from] TransactionRequestError),
}

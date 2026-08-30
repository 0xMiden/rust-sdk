use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use async_trait::async_trait;
use miden_protocol::Word;
use miden_protocol::account::{AccountCode, AccountId};
use miden_protocol::block::{BlockNumber, FeeParameters};
use miden_protocol::note::{Note, NoteId};
use miden_standards::account::auth::{FeeConversionInfo, commit_fee_conversion_info};
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
    auth_component_of,
    reads_auth_args_as_fee_conversion_info,
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
    ///
    /// On a chain that charges a fee, screening adds an auth arg committing the chain's native
    /// asset at rate 1/1 for the accounts whose auth component reads it as fee conversion info,
    /// since the trial execution pays the fee. Auth args set here are kept as they are.
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
        let fee_parameters = self.fee_parameters(block_ref).await?;

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

            let account_tx_args = with_native_fee_conversion_info(
                tx_args.clone(),
                account_id,
                &account_code,
                fee_parameters.as_ref(),
            );

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
        let tx_args = with_native_fee_conversion_info(
            self.tx_args(),
            account_id,
            &account_code,
            self.fee_parameters(block_ref).await?.as_ref(),
        );

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

    /// Returns the fee parameters in force at `block_ref`, or `None` when that header is not in
    /// the store.
    ///
    /// The `None` is defensive: the trial execution below reads the same header through the data
    /// store, so it would fail on its own were the header missing. Reporting it from here would
    /// only replace that error with a less specific one.
    async fn fee_parameters(
        &self,
        block_ref: BlockNumber,
    ) -> Result<Option<FeeParameters>, NoteScreenerError> {
        Ok(self
            .store
            .get_block_header_by_num(block_ref)
            .await?
            .map(|(header, _)| header.fee_parameters().clone()))
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

// HELPER FUNCTIONS
// ================================================================================================

/// Commits the chain's native fee conversion info in the auth args when the account's auth
/// component reads them as such.
///
/// Screening runs the account's auth procedure, and on a fee-charging chain
/// `miden::standards::fee::pay_fee` aborts with `ERR_FEE_CONVERSION_INFO_MISSING` when a non-zero
/// fee meets empty conversion info. [`NoteConsumptionChecker`] reports that abort as
/// [`NoteConsumptionStatus::UnconsumableConditions`], which would make every note screen as
/// irrelevant and drop it from the sync. Committing the same asset and rate the execution path
/// attaches lets the check measure the note against the fee the real transaction would pay.
///
/// `AuthMultisig` and `AuthGuardedMultisig` get the native rate here even though
/// [`Client::execute_transaction`](crate::Client::execute_transaction) makes their accounts declare
/// their own: screening only asks whether the note could be consumed, and the native rate answers
/// that far better than refusing to look. Both reuse the auth-arg word as their transaction summary
/// salt, which is why a real send needs a salt only the caller can choose. The constant is safe
/// here because the screening executor carries no authenticator, so the trial stops at the
/// signature request inside `multisig::auth_tx` and never reaches
/// `multisig::record_and_assert_new_tx`, which is what a reused salt would collide in.
fn with_native_fee_conversion_info(
    tx_args: TransactionArgs,
    account_id: AccountId,
    account_code: &AccountCode,
    fee_parameters: Option<&FeeParameters>,
) -> TransactionArgs {
    let Some(fee_parameters) = fee_parameters else {
        return tx_args;
    };

    if fee_parameters.verification_base_fee() == 0 {
        return tx_args;
    }

    // Auth args the caller set are the caller's business, as on the execution path: they may
    // already commit conversion info for a non-native asset, or carry something else entirely.
    if tx_args.auth_args() != Word::empty() {
        return tx_args;
    }

    if !auth_component_of(&account_code.interface(account_id))
        .as_ref()
        .is_some_and(reads_auth_args_as_fee_conversion_info)
    {
        return tx_args;
    }

    let (auth_arg, preimage) = commit_fee_conversion_info(
        FeeConversionInfo::one_to_one(fee_parameters.fee_faucet_id()),
        NATIVE_FEE_CONVERSION_SALT,
    );

    let mut tx_args = tx_args.with_auth_args(auth_arg);
    tx_args.extend_advice_map([(auth_arg, preimage)]);
    tx_args
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

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use miden_protocol::account::{
        Account,
        AccountBuilder,
        AccountComponent,
        AccountId,
        AccountType,
    };
    use miden_protocol::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;
    use miden_standards::account::AccountBuilderSchemaCommitmentExt;
    use miden_standards::account::auth::{
        Approver,
        ApproverSet,
        AuthGuardedMultisig,
        AuthGuardedMultisigConfig,
        AuthMultisig,
        AuthMultisigConfig,
        AuthMultisigSmart,
        AuthMultisigSmartConfig,
        AuthSingleSig,
        GuardianConfig,
        NoAuth,
    };
    use miden_standards::account::wallets::BasicWallet;

    use super::*;
    use crate::auth::{AuthSchemeId, AuthSecretKey};

    const NATIVE_FEE_FAUCET: u128 = ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;

    fn native_fee_faucet_id() -> AccountId {
        AccountId::try_from(NATIVE_FEE_FAUCET).unwrap()
    }

    fn fee_parameters(verification_base_fee: u32) -> FeeParameters {
        FeeParameters::new(native_fee_faucet_id(), verification_base_fee)
    }

    fn account_with_auth(auth_component: impl Into<AccountComponent>) -> Account {
        AccountBuilder::new([7u8; 32])
            .account_type(AccountType::Public)
            .with_component(auth_component)
            .with_component(BasicWallet)
            .build_with_schema_commitment()
            .expect("account creation failed")
    }

    fn singlesig_account() -> Account {
        let key = AuthSecretKey::new_falcon512_poseidon2();
        account_with_auth(AuthSingleSig::new(Approver::new(
            key.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )))
    }

    fn multisig_account() -> Account {
        let approvers = ApproverSet::new(
            vec![Approver::new(
                AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )],
            1,
        )
        .unwrap();

        account_with_auth(AuthMultisig::new(AuthMultisigConfig::new(approvers)).unwrap())
    }

    fn guarded_multisig_account() -> Account {
        let approvers = ApproverSet::new(
            vec![Approver::new(
                AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )],
            1,
        )
        .unwrap();

        let guardian = GuardianConfig::new(Approver::new(
            AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        ));

        account_with_auth(
            AuthGuardedMultisig::new(AuthGuardedMultisigConfig::new(approvers, guardian).unwrap())
                .unwrap(),
        )
    }

    fn multisig_smart_account() -> Account {
        let approvers = ApproverSet::new(
            vec![Approver::new(
                AuthSecretKey::new_falcon512_poseidon2().public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            )],
            1,
        )
        .unwrap();

        account_with_auth(AuthMultisigSmart::new(AuthMultisigSmartConfig::new(approvers)).unwrap())
    }

    /// Returns the auth args a screening pass would run `account` under.
    fn screening_auth_args(
        account: &Account,
        verification_base_fee: u32,
        preset: Option<Word>,
    ) -> Word {
        let tx_args = match preset {
            Some(auth_args) => TransactionArgs::new(AdviceMap::default()).with_auth_args(auth_args),
            None => TransactionArgs::new(AdviceMap::default()),
        };

        with_native_fee_conversion_info(
            tx_args,
            account.id(),
            account.code(),
            Some(&fee_parameters(verification_base_fee)),
        )
        .auth_args()
    }

    fn native_commitment() -> Word {
        commit_fee_conversion_info(
            FeeConversionInfo::one_to_one(native_fee_faucet_id()),
            NATIVE_FEE_CONVERSION_SALT,
        )
        .0
    }

    /// Trial execution runs the auth procedure, so without conversion info `fee::pay_fee` aborts
    /// and the note screens as unconsumable. The commitment has to match the one the execution
    /// path attaches, or screening would measure a fee the real transaction never pays.
    #[test]
    fn a_fee_charging_chain_gets_native_conversion_info_for_the_auth_components_reading_it() {
        for account in [singlesig_account(), multisig_account(), guarded_multisig_account()] {
            let component = auth_component_of(&account.code().interface(account.id()));
            assert_eq!(
                screening_auth_args(&account, 500, None),
                native_commitment(),
                "screening should commit the native asset at rate 1/1 for {component:?}"
            );
        }
    }

    /// The advice map has to carry the preimage: the auth procedure resolves the commitment
    /// through it, and an entry-less map aborts just as a missing commitment does.
    #[test]
    fn the_conversion_info_preimage_is_placed_in_the_advice_map() {
        let (auth_arg, expected_preimage) = commit_fee_conversion_info(
            FeeConversionInfo::one_to_one(native_fee_faucet_id()),
            NATIVE_FEE_CONVERSION_SALT,
        );

        for account in [singlesig_account(), multisig_account(), guarded_multisig_account()] {
            let component = auth_component_of(&account.code().interface(account.id()));
            let tx_args = with_native_fee_conversion_info(
                TransactionArgs::new(AdviceMap::default()),
                account.id(),
                account.code(),
                Some(&fee_parameters(500)),
            );

            let (_, advice_map, _) = tx_args.advice_inputs().clone().into_parts();
            assert_eq!(
                advice_map.get(&auth_arg).map(|preimage| preimage.to_vec()),
                Some(expected_preimage.clone()),
                "the commitment should resolve to its preimage during screening for {component:?}"
            );
        }
    }

    #[test]
    fn a_chain_charging_nothing_leaves_the_auth_args_alone() {
        assert_eq!(
            screening_auth_args(&singlesig_account(), 0, None),
            Word::empty(),
            "a fee-free chain needs no conversion info"
        );
    }

    /// `NoAuth` takes its asset and rate from `fee::native_conversion_info` instead of the auth
    /// args, so an injected commitment would be discarded. `AuthMultisigSmart` never reaches
    /// `miden::standards::fee` at all and would read the commitment as its summary salt.
    #[test]
    fn an_auth_component_that_does_not_read_the_auth_args_is_left_alone() {
        for account in [account_with_auth(NoAuth), multisig_smart_account()] {
            let component = auth_component_of(&account.code().interface(account.id()));
            assert_eq!(
                screening_auth_args(&account, 500, None),
                Word::empty(),
                "only the components reading the auth args as conversion info should get them, \
                 but {component:?} did"
            );
        }
    }

    /// Caller-supplied auth args may already commit a non-native asset, or carry something else
    /// entirely, so screening defers to them exactly as the execution path does.
    #[test]
    fn caller_supplied_auth_args_are_preserved() {
        let preset = Word::from([1u32, 2, 3, 4]);
        assert_eq!(
            screening_auth_args(&singlesig_account(), 500, Some(preset)),
            preset,
            "screening should not overwrite auth args the caller set"
        );
    }

    /// Absent fee parameters are the defensive branch described on
    /// [`NoteScreener::fee_parameters`]: nothing is committed, and the trial execution reports the
    /// missing header itself.
    #[test]
    fn absent_fee_parameters_leave_the_auth_args_alone() {
        let account = singlesig_account();
        assert_eq!(
            with_native_fee_conversion_info(
                TransactionArgs::new(AdviceMap::default()),
                account.id(),
                account.code(),
                None,
            )
            .auth_args(),
            Word::empty(),
        );
    }
}

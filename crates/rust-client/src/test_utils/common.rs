use std::boxed::Box;
use std::collections::BTreeMap;
use std::env::temp_dir;
use std::fs::OpenOptions;
use std::io::Write;
use std::ops::{Deref, DerefMut};
use std::path::PathBuf;
use std::string::ToString;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::vec::Vec;

use anyhow::{Context, Result};
use miden_protocol::account::auth::AuthSecretKey;
use miden_protocol::account::{Account, AccountId};
use miden_protocol::asset::{AssetAmount, FungibleAsset, TokenSymbol};
use miden_protocol::note::NoteType;
use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;
use miden_protocol::transaction::TransactionId;
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::faucets::TokenName;
use rand::Rng;
use tracing::{debug, info};
use uuid::Uuid;

use crate::account::component::{
    AccountComponent,
    BasicWallet,
    BurnPolicy,
    FungibleFaucet,
    MintPolicy,
    TokenPolicyManager,
};
use crate::account::{AccountBuilder, AccountBuilderSchemaCommitmentExt, AccountType};
use crate::auth::{AuthSchemeId, RPO_FALCON_SCHEME_ID};
pub use crate::keystore::{FilesystemKeyStore, Keystore};
use crate::note::{Note, NoteConsumability, P2idNote};
use crate::rpc::RpcError;
use crate::store::{InputNoteRecord, NoteFilter, TransactionFilter};
use crate::sync::SyncSummary;
use crate::test_utils::fee::FeeFunder;
use crate::transaction::{
    NoteArgs,
    TransactionRequest,
    TransactionRequestBuilder,
    TransactionRequestError,
    TransactionResult,
    TransactionStatus,
};
use crate::{Client, ClientError};

// TEST CLIENT
// ================================================================================================

/// A [`Client`] wired for the test helpers, carrying the [`FeeFunder`] the account-creating helpers
/// pay deploys from when the chain charges transaction fees.
///
/// Dereferences to the wrapped [`Client`], so it is used exactly like one.
pub struct TestClient {
    client: Client<FilesystemKeyStore>,
    fee_funder: Option<Arc<dyn FeeFunder>>,
    /// Funding notes paid to accounts that have not spent them yet.
    ///
    /// A note's assets reach the vault before the fee is withdrawn, so folding one into an
    /// account's next transaction makes that transaction its deploy as well.
    pending_funding: BTreeMap<AccountId, Note>,
}

impl TestClient {
    /// Wraps `client` with no fee funder, which is all a fee-free chain needs.
    pub fn new(client: Client<FilesystemKeyStore>) -> Self {
        Self {
            client,
            fee_funder: None,
            pending_funding: BTreeMap::new(),
        }
    }

    /// Returns the keystore the client signs with, shared with it through the authenticator.
    pub fn keystore(&self) -> &FilesystemKeyStore {
        self.client
            .authenticator()
            .expect("test clients are always built with a keystore authenticator")
            .as_ref()
    }

    /// Records funding notes, to be folded into each account's next transaction.
    pub(crate) fn stash_funding(&mut self, funded: impl IntoIterator<Item = (AccountId, Note)>) {
        self.pending_funding.extend(funded);
    }

    /// Takes `account_id`'s funding note, opting it out of automatic folding.
    ///
    /// The caller must then consume it somewhere, or the account cannot pay a fee. Use it when a
    /// test needs the funding in a particular transaction — one asserting on what a sync reports,
    /// say.
    pub fn take_funding(&mut self, account_id: AccountId) -> Option<Note> {
        self.pending_funding.remove(&account_id)
    }

    /// Submits a transaction for `account_id`, folding in its funding note when it has one.
    ///
    /// Shadows [`Client::submit_new_transaction`], still reachable through [`Deref`] for callers
    /// that want the unfunded path.
    pub async fn submit_new_transaction(
        &mut self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
    ) -> Result<TransactionId, ClientError> {
        let transaction_request = self.fund_request(account_id, transaction_request);

        Box::pin(self.client.submit_new_transaction(account_id, transaction_request)).await
    }

    /// Executes a transaction for `account_id`, folding in its funding note when it has one.
    ///
    /// Takes `&mut self` where the wrapped method takes `&self`, since taking the note mutates.
    pub async fn execute_transaction(
        &mut self,
        account_id: AccountId,
        transaction_request: TransactionRequest,
    ) -> Result<TransactionResult, ClientError> {
        let transaction_request = self.fund_request(account_id, transaction_request);

        Box::pin(self.client.execute_transaction(account_id, transaction_request)).await
    }

    /// Returns `transaction_request` with `account_id`'s funding note folded in.
    ///
    /// Only needed for requests not going through [`Self::submit_new_transaction`] — notably a
    /// batch, which borrows the client, so the note must be taken before the batch is created.
    #[must_use]
    pub fn fund_request(
        &mut self,
        account_id: AccountId,
        mut transaction_request: TransactionRequest,
    ) -> TransactionRequest {
        if let Some(note) = self.take_funding(account_id) {
            transaction_request.add_unauthenticated_input_note(note);
        }

        transaction_request
    }

    /// Sets the funder the account-creating helpers draw the native fee asset from.
    #[must_use]
    pub fn with_fee_funder(mut self, fee_funder: Option<Arc<dyn FeeFunder>>) -> Self {
        self.fee_funder = fee_funder;
        self
    }

    /// Returns the fee funder, if one is set.
    pub fn fee_funder(&self) -> Option<&Arc<dyn FeeFunder>> {
        self.fee_funder.as_ref()
    }
}

impl From<Client<FilesystemKeyStore>> for TestClient {
    fn from(client: Client<FilesystemKeyStore>) -> Self {
        Self::new(client)
    }
}

impl Deref for TestClient {
    type Target = Client<FilesystemKeyStore>;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl DerefMut for TestClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.client
    }
}

// ACCOUNT SETUP
// ================================================================================================

/// What kind of standard components a test account is built around.
enum AccountKind {
    /// A [`BasicWallet`].
    Wallet,
    /// A [`FungibleFaucet`] with permissive mint/burn policies, plus a [`BasicWallet`] for its
    /// `receive_asset` procedure, which `FungibleFaucet` does not export, so a P2ID note can fund
    /// the faucet's own minting fees. Minting is unaffected.
    Faucet,
}

/// Configuration for an account inserted through [`TestClient::insert_account`].
pub struct AccountSetup {
    kind: AccountKind,
    account_type: AccountType,
    auth_scheme: AuthSchemeId,
    seed: Option<[u8; 32]>,
    funded: bool,
    bare: bool,
    extra_components: Vec<AccountComponent>,
}

impl AccountSetup {
    fn new(kind: AccountKind, account_type: AccountType) -> Self {
        Self {
            kind,
            account_type,
            auth_scheme: RPO_FALCON_SCHEME_ID,
            seed: None,
            funded: true,
            bare: false,
            extra_components: Vec::new(),
        }
    }

    /// A basic wallet account.
    pub fn wallet(account_type: AccountType) -> Self {
        Self::new(AccountKind::Wallet, account_type)
    }

    /// A fungible faucet account.
    pub fn faucet(account_type: AccountType) -> Self {
        Self::new(AccountKind::Faucet, account_type)
    }

    /// Signs with `auth_scheme` instead of the default [`RPO_FALCON_SCHEME_ID`].
    #[must_use]
    pub fn auth_scheme(mut self, auth_scheme: AuthSchemeId) -> Self {
        self.auth_scheme = auth_scheme;
        self
    }

    /// Builds the account from `seed` instead of a random one, for tests that re-derive the
    /// account ID.
    #[must_use]
    pub fn seed(mut self, seed: [u8; 32]) -> Self {
        self.seed = Some(seed);
        self
    }

    /// Skips funding the account on insertion.
    #[must_use]
    pub fn unfunded(mut self) -> Self {
        self.funded = false;
        self
    }

    /// Builds a faucet without the [`BasicWallet`] ride-along, so the account exposes only the
    /// faucet interface.
    #[must_use]
    pub fn bare(mut self) -> Self {
        self.bare = true;
        self
    }

    /// Adds `component` on top of the standard ones.
    #[must_use]
    pub fn component(mut self, component: AccountComponent) -> Self {
        self.extra_components.push(component);
        self
    }
}

impl TestClient {
    /// Builds the account described by `setup`, adds its key to the keystore, and inserts it into
    /// the client. Unless [`AccountSetup::unfunded`] was set, the account is also funded so its
    /// first transaction can pay its own fee and double as its deploy.
    pub async fn insert_account(
        &mut self,
        setup: AccountSetup,
    ) -> Result<(Account, AuthSecretKey)> {
        let key_pair = match setup.auth_scheme {
            AuthSchemeId::Falcon512Poseidon2 => AuthSecretKey::new_falcon512_poseidon2(),
            AuthSchemeId::EcdsaK256Keccak => AuthSecretKey::new_ecdsa_k256_keccak(),
            other => anyhow::bail!("unsupported auth scheme: {}", other.as_u8()),
        };
        let auth_component = AuthSingleSig::new(Approver::new(
            key_pair.public_key().to_commitment(),
            setup.auth_scheme,
        ));

        let init_seed = setup.seed.unwrap_or_else(|| {
            let mut seed = [0u8; 32];
            self.rng().fill_bytes(&mut seed);
            seed
        });

        let mut builder = AccountBuilder::new(init_seed)
            .account_type(setup.account_type)
            .with_component(auth_component);

        match setup.kind {
            AccountKind::Wallet => {
                builder = builder.with_component(BasicWallet);
            },
            AccountKind::Faucet => {
                let symbol = TokenSymbol::new("TEST").expect("TEST is a valid token symbol");
                let name = TokenName::new(&symbol.to_string())
                    .expect("token symbol is a valid token name");
                let max_supply = 9_999_999_u64;
                let faucet = FungibleFaucet::builder()
                    .name(name)
                    .symbol(symbol)
                    .decimals(10)
                    .max_supply(AssetAmount::new(max_supply).expect("max supply is a valid amount"))
                    .build()
                    .context("failed to build the fungible faucet component")?;

                let policy_manager = TokenPolicyManager::builder()
                    .active_mint_policy(MintPolicy::allow_all())
                    .active_burn_policy(BurnPolicy::allow_all())
                    .build();

                builder = builder.with_component(faucet);
                if !setup.bare {
                    builder = builder.with_component(BasicWallet);
                }
                builder = builder.with_components(policy_manager);
            },
        }

        for component in setup.extra_components {
            builder = builder.with_component(component);
        }

        let account = builder
            .build_with_schema_commitment()
            .context("failed to build the test account")?;

        self.keystore()
            .add_key(&key_pair, account.id())
            .await
            .context("failed to add the account key to the keystore")?;

        self.add_account(&account, false).await?;

        info!(account_id = %account.id(), account_type = ?setup.account_type, "Inserted account");

        if setup.funded {
            self.fund_if_needed(&[account.id()]).await?;
        }

        Ok((account, key_pair))
    }

    /// Inserts a new funded wallet account, signing with the default auth scheme.
    pub async fn insert_wallet(&mut self, account_type: AccountType) -> Result<Account> {
        let (account, _) = self.insert_account(AccountSetup::wallet(account_type)).await?;
        Ok(account)
    }

    /// Inserts a new funded fungible faucet account, signing with the default auth scheme.
    pub async fn insert_faucet(&mut self, account_type: AccountType) -> Result<Account> {
        let (account, _) = self.insert_account(AccountSetup::faucet(account_type)).await?;
        Ok(account)
    }

    /// Sets up a wallet account and a faucet account (in that order).
    pub async fn setup_wallet_and_faucet(
        &mut self,
        account_type: AccountType,
    ) -> Result<(Account, Account)> {
        let (faucet_account, _) = self
            .insert_account(AccountSetup::faucet(account_type).unfunded())
            .await
            .context("failed to insert new fungible faucet account")?;

        let (basic_account, _) = self
            .insert_account(AccountSetup::wallet(account_type).unfunded())
            .await
            .context("failed to insert new wallet account")?;

        self.fund_if_needed(&[faucet_account.id(), basic_account.id()])
            .await
            .context("failed to fund and deploy the created accounts")?;

        Ok((basic_account, faucet_account))
    }

    /// Sets up two wallet accounts and a faucet account (in that order), on a client that has to
    /// be in a clean state.
    pub async fn setup_two_wallets_and_faucet(
        &mut self,
        account_type: AccountType,
    ) -> Result<(Account, Account, Account)> {
        // Ensure clean state
        let account_headers = self
            .get_account_headers()
            .await
            .with_context(|| "failed to get account headers")?;
        anyhow::ensure!(
            account_headers.is_empty(),
            "Expected empty account headers for clean state"
        );

        let transactions = self
            .get_transactions(TransactionFilter::All)
            .await
            .with_context(|| "failed to get transactions")?;
        anyhow::ensure!(transactions.is_empty(), "Expected empty transactions for clean state");

        let input_notes = self
            .get_input_notes(NoteFilter::All)
            .await
            .with_context(|| "failed to get input notes")?;
        anyhow::ensure!(input_notes.is_empty(), "Expected empty input notes for clean state");

        let (faucet_account, _) = self
            .insert_account(AccountSetup::faucet(account_type).unfunded())
            .await
            .context("failed to insert new fungible faucet account")?;

        let (first_basic_account, _) = self
            .insert_account(AccountSetup::wallet(account_type).unfunded())
            .await
            .context("failed to insert first basic wallet account")?;

        let (second_basic_account, _) = self
            .insert_account(AccountSetup::wallet(account_type).unfunded())
            .await
            .context("failed to insert second basic wallet account")?;

        self.fund_if_needed(&[
            faucet_account.id(),
            first_basic_account.id(),
            second_basic_account.id(),
        ])
        .await
        .context("failed to fund and deploy the created accounts")?;

        info!(
            faucet_id = %faucet_account.id(),
            wallet_1_id = %first_basic_account.id(),
            wallet_2_id = %second_basic_account.id(),
            "Setup complete, syncing state"
        );
        self.sync_state().await.with_context(|| "failed to sync client state")?;

        Ok((first_basic_account, second_basic_account, faucet_account))
    }
}

// TRANSACTION HELPERS
// ================================================================================================

impl TestClient {
    /// Executes a transaction and asserts that it fails with the expected error.
    pub async fn execute_failing_tx(
        &mut self,
        account_id: AccountId,
        tx_request: TransactionRequest,
        expected_error: ClientError,
    ) {
        info!(account_id = %account_id, "Executing transaction (expecting failure)");
        // We compare string since we can't compare the error directly
        assert_eq!(
            self.submit_new_transaction(account_id, tx_request)
                .await
                .unwrap_err()
                .to_string(),
            expected_error.to_string()
        );
    }

    /// Executes a transaction and waits for it to be committed.
    pub async fn execute_tx_and_sync(
        &mut self,
        account_id: AccountId,
        tx_request: TransactionRequest,
    ) -> Result<()> {
        let transaction_id = self.submit_new_transaction(account_id, tx_request).await?;
        info!(tx_id = %transaction_id, account_id = %account_id, "Transaction submitted, waiting for commit");
        self.wait_for_tx(transaction_id).await?;
        Ok(())
    }

    /// Syncs the client and waits for the transaction to be committed.
    pub async fn wait_for_tx(&mut self, transaction_id: TransactionId) -> Result<()> {
        // wait until tx is committed
        let now = Instant::now();
        debug!(tx_id = %transaction_id, "Waiting for transaction to be committed");
        loop {
            self.sync_state()
                .await
                .with_context(|| "failed to sync client state while waiting for transaction")?;

            // Check if executed transaction got committed by the node
            let tracked_transaction = self
                .get_transactions(TransactionFilter::Ids(vec![transaction_id]))
                .await
                .with_context(|| format!("failed to get transaction with ID: {transaction_id}"))?
                .pop()
                .with_context(|| format!("transaction with ID {transaction_id} not found"))?;

            match tracked_transaction.status {
                TransactionStatus::Committed { block_number, .. } => {
                    info!(tx_id = %transaction_id, %block_number, "Transaction committed");
                    break;
                },
                TransactionStatus::Pending => {
                    // Cooldown between polling iterations to reduce pressure on the node's
                    // rate limiter when many integration tests poll concurrently.
                    tokio::time::sleep(Duration::from_millis(500)).await;
                },
                TransactionStatus::Discarded(cause) => {
                    anyhow::bail!("transaction was discarded with cause: {cause:?}");
                },
            }

            // Log wait time in a file if the env var is set
            // This allows us to aggregate and measure how long the tests are waiting for
            // transactions to be committed
            if std::env::var("LOG_WAIT_TIMES") == Ok("true".to_string()) {
                let elapsed = now.elapsed();
                let wait_times_dir = std::path::PathBuf::from("wait_times");
                std::fs::create_dir_all(&wait_times_dir)
                    .with_context(|| "failed to create wait_times directory")?;

                let elapsed_time_file =
                    wait_times_dir.join(format!("wait_time_{}", Uuid::new_v4()));
                let mut file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(elapsed_time_file)
                    .with_context(|| "failed to create elapsed time file")?;
                writeln!(file, "{:?}", elapsed.as_millis())
                    .with_context(|| "failed to write elapsed time to file")?;
            }
        }
        Ok(())
    }

    /// Syncs until `amount_of_blocks` have been created onchain compared to client's sync height.
    pub async fn wait_for_blocks(&mut self, amount_of_blocks: u32) -> Result<SyncSummary> {
        let current_block = self.get_sync_height().await?;
        let final_block = current_block + amount_of_blocks;
        debug!(current_block = %current_block, target_block = %final_block, "Waiting for blocks");
        loop {
            let summary = self.sync_state().await?;
            debug!(sync_height = %summary.block_num, target_block = %final_block, "Synced");

            if summary.block_num >= final_block {
                return Ok(summary);
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    /// Idles until `amount_of_blocks` have been created onchain compared to client's sync height
    /// without advancing the client's sync height.
    pub async fn wait_for_blocks_no_sync(&mut self, amount_of_blocks: u32) -> Result<()> {
        let current_block = self.get_sync_height().await?;
        let final_block = current_block + amount_of_blocks;
        debug!(current_block = %current_block, target_block = %final_block, "Waiting for blocks (no sync)");
        loop {
            let (latest_block, _) =
                self.test_rpc_api().get_block_header_by_number(None, false).await?;
            debug!(
                chain_tip = %latest_block.block_num(),
                target_block = %final_block,
                "Waiting for blocks (no sync)"
            );

            if latest_block.block_num() >= final_block {
                return Ok(());
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    /// Syncs repeatedly until the given account has at least one consumable note, or until
    /// `max_blocks` have elapsed since the call. Returns the list of consumable notes once found.
    pub async fn wait_for_consumable_notes(
        &mut self,
        account_id: AccountId,
        max_blocks: u32,
    ) -> Result<Vec<(InputNoteRecord, Vec<NoteConsumability>)>> {
        let start_block = self.get_sync_height().await?;
        let deadline_block = start_block + max_blocks;
        debug!(
            %account_id,
            %start_block,
            %deadline_block,
            "Waiting for consumable notes"
        );

        loop {
            self.sync_state().await?;
            let notes = self.get_consumable_notes(Some(account_id)).await?;
            if !notes.is_empty() {
                let current_block = self.get_sync_height().await?;
                debug!(
                    %account_id,
                    count = notes.len(),
                    %current_block,
                    "Found consumable notes"
                );
                return Ok(notes);
            }

            let current_block = self.get_sync_height().await?;
            assert!(
                current_block < deadline_block,
                "account {account_id} has no consumable notes after waiting {max_blocks} blocks \
                 (from block {start_block} to {current_block})"
            );

            debug!(
                %account_id,
                %current_block,
                %deadline_block,
                "No consumable notes yet, waiting..."
            );
            std::thread::sleep(Duration::from_secs(3));
        }
    }

    /// Waits for node to be running.
    pub async fn wait_for_node(&mut self) {
        const NODE_TIME_BETWEEN_ATTEMPTS: u64 = 2;
        const NUMBER_OF_NODE_ATTEMPTS: u64 = 60;
        info!(
            "Waiting for node to be up (checking every {NODE_TIME_BETWEEN_ATTEMPTS}s, max {NUMBER_OF_NODE_ATTEMPTS} tries)"
        );
        for _try_number in 0..NUMBER_OF_NODE_ATTEMPTS {
            match self.sync_state().await {
                Err(ClientError::RpcError(
                    RpcError::ConnectionError(_) | RpcError::RequestError { .. },
                )) => {
                    tokio::time::sleep(Duration::from_secs(NODE_TIME_BETWEEN_ATTEMPTS)).await;
                },
                Err(other_error) => {
                    panic!("Unexpected error: {other_error}");
                },
                _ => return,
            }
        }

        panic!("Unable to connect to node");
    }

    /// Mints a note from `faucet_account_id` for `basic_account_id` and returns the executed
    /// transaction ID and the note with [`MINT_AMOUNT`] units of the corresponding fungible
    /// asset.
    pub async fn mint_note(
        &mut self,
        basic_account_id: AccountId,
        faucet_account_id: AccountId,
        note_type: NoteType,
    ) -> Result<(TransactionId, Note)> {
        // Create a Mint Tx for MINT_AMOUNT units of our fungible asset
        let fungible_asset = FungibleAsset::new(faucet_account_id, MINT_AMOUNT)?;
        info!(faucet_id = %faucet_account_id, target_id = %basic_account_id, amount = MINT_AMOUNT, "Minting asset");
        let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            fungible_asset,
            basic_account_id,
            note_type,
            self.rng(),
        )?;
        let tx_id = self
            .submit_new_transaction(fungible_asset.faucet_id(), tx_request.clone())
            .await?;

        let note = tx_request
            .expected_output_own_notes()
            .pop()
            .context("the mint request should produce one output note")?;
        info!(tx_id = %tx_id, note_id = %note.id(), "Mint transaction submitted");
        Ok((tx_id, note))
    }

    /// Executes a transaction that consumes the provided notes and returns the transaction ID.
    /// This assumes the notes contain assets.
    pub async fn consume_notes(
        &mut self,
        account_id: AccountId,
        input_notes: &[Note],
    ) -> Result<TransactionId> {
        let note_ids: Vec<_> = input_notes.iter().map(|n| n.id().to_string()).collect();
        info!(account_id = %account_id, note_ids = %note_ids.join(", "), "Consuming notes");
        let tx_request =
            TransactionRequestBuilder::new().build_consume_notes(input_notes.to_vec())?;
        let tx_id = self.submit_new_transaction(account_id, tx_request).await?;
        info!(tx_id = %tx_id, "Consume transaction submitted");
        Ok(tx_id)
    }

    /// Executes a transaction and consumes the resulting unauthenticated notes immediately
    /// without waiting for the first transaction to be committed.
    pub async fn execute_tx_and_consume_output_notes(
        &mut self,
        tx_request: TransactionRequest,
        executor: AccountId,
        consumer: AccountId,
    ) -> Result<TransactionId> {
        let output_notes = tx_request
            .expected_output_own_notes()
            .into_iter()
            .map(|note| (note, None::<NoteArgs>))
            .collect::<Vec<(Note, Option<NoteArgs>)>>();

        self.submit_new_transaction(executor, tx_request).await?;

        let tx_request = TransactionRequestBuilder::new().input_notes(output_notes).build()?;
        Ok(self.submit_new_transaction(consumer, tx_request).await?)
    }

    /// Mints assets for the target account and consumes them immediately without waiting for the
    /// first transaction to be committed.
    pub async fn mint_and_consume(
        &mut self,
        basic_account_id: AccountId,
        faucet_account_id: AccountId,
        note_type: NoteType,
    ) -> Result<TransactionId> {
        info!(
            faucet_id = %faucet_account_id,
            target_id = %basic_account_id,
            amount = MINT_AMOUNT,
            "Minting and consuming asset"
        );
        let tx_request = TransactionRequestBuilder::new().build_mint_fungible_asset(
            FungibleAsset::new(faucet_account_id, MINT_AMOUNT)?,
            basic_account_id,
            note_type,
            self.rng(),
        )?;

        let tx_id = self
            .execute_tx_and_consume_output_notes(tx_request, faucet_account_id, basic_account_id)
            .await?;
        info!(tx_id = %tx_id, "Mint-and-consume transaction submitted");
        Ok(tx_id)
    }

    /// Creates a transaction request that mints assets for each `target_id` account.
    pub fn mint_multiple_fungible_asset(
        &mut self,
        asset: FungibleAsset,
        target_id: &[AccountId],
        note_type: NoteType,
    ) -> Result<TransactionRequest> {
        let rng = self.rng();
        let notes = target_id
            .iter()
            .map(|account_id| {
                Ok(P2idNote::builder()
                    .sender(asset.faucet_id())
                    .target(*account_id)
                    .asset(asset)
                    .note_type(note_type)
                    .generate_serial_number(rng)
                    .build()
                    .context("note creation failed")?
                    .into())
            })
            .collect::<Result<Vec<Note>>>()?;

        Ok(TransactionRequestBuilder::new().own_output_notes(notes).build()?)
    }
}

// ASSERTION HELPERS
// ================================================================================================

impl TestClient {
    /// Asserts that the account has a single asset with the expected amount.
    pub async fn assert_account_has_single_asset(
        &self,
        account_id: AccountId,
        faucet_id: AccountId,
        expected_amount: u64,
    ) {
        let balance = self
            .account_reader(account_id)
            .get_balance(faucet_id)
            .await
            .expect("Account should have the asset");
        assert_eq!(balance, AssetAmount::new(expected_amount).unwrap());
    }

    /// Tries to consume the note and asserts that the expected error is returned.
    pub async fn assert_note_cannot_be_consumed_twice(
        &mut self,
        consuming_account_id: AccountId,
        note_to_consume: Note,
    ) {
        // Check that we can't consume the P2ID note again
        info!(note_id = %note_to_consume.id(), account_id = %consuming_account_id, "Attempting double-consume (expecting failure)");

        // Double-spend error expected to be received since we are consuming the same note
        let tx_request = TransactionRequestBuilder::new()
            .build_consume_notes(vec![note_to_consume.clone()])
            .unwrap();

        match self.submit_new_transaction(consuming_account_id, tx_request).await {
            Err(ClientError::TransactionRequestError(
                TransactionRequestError::InputNoteAlreadyConsumed(_),
            )) => {},
            Ok(_) => panic!("Double-spend error: Note should not be consumable!"),
            err => {
                panic!("Unexpected error {:?} for note ID: {}", err, note_to_consume.id().to_hex())
            },
        }
    }
}

// CONSTANTS
// ================================================================================================

pub const ACCOUNT_ID_REGULAR: u128 = ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;

/// Constant that represents the number of blocks until the p2id can be recalled. If this value is
/// too low, some tests might fail due to expected recall failures not happening.
pub const RECALL_HEIGHT_DELTA: u32 = 50;

pub const MINT_AMOUNT: u64 = 1000;
pub const TRANSFER_AMOUNT: u64 = 59;

// UTILITIES
// ================================================================================================

pub fn create_test_store_path() -> PathBuf {
    let mut temp_file = temp_dir();
    temp_file.push(format!("{}.sqlite3", Uuid::new_v4()));
    temp_file
}

//! Tests for re-deriving a multisig transaction summary at the chain tip.
//!
//! A multisig proposal is signed over a [`TransactionSummary`] that binds a caller-chosen block
//! through [`MultisigAuthArgs`] (protocol 0.16+), not the execution reference block. That is what
//! lets the party executing an already-signed proposal move execution to the current tip, so that
//! foreign account state is fetched at a block the node still serves, while the summary commitment
//! the approvers signed is reproduced unchanged.
//!
//! The motivating case is the chain fee faucet, which a fee-paying transaction loads through the
//! kernel; more generally it is any foreign account the request reads through FPI, since a foreign
//! account's proof is fetched at the execution reference block. To exercise that fetch
//! deterministically these tests declare an explicit public foreign account on the request (the
//! `MockChain` harness resolves the fee faucet itself without a `get_account` call), then prune the
//! node's account state at the proposal's bound block.
//!
//! Executing a multisig request without collected signatures deliberately fails with
//! [`TransactionExecutorError::Unauthorized`], which carries the [`TransactionSummary`] to be signed.
//! These tests read that summary to compare commitments across execution reference blocks. The
//! foreign account is fetched before the summary is built, so reaching the summary at all proves the
//! foreign load succeeded.
//!
//! They run against a `MockChain` with a non-zero `verification_base_fee`, the switch that turns fee
//! collection on, so a fee note is built into the summary.
//!
//! [`TransactionSummary`]: miden_protocol::transaction::TransactionSummary
//! [`MultisigAuthArgs`]: miden_client::auth::MultisigAuthArgs
//! [`TransactionExecutorError::Unauthorized`]: miden_client::transaction::TransactionExecutorError

use std::collections::BTreeSet;
use std::env::temp_dir;
use std::sync::{Arc, Mutex};

use miden_client::account::component::FeeConversionInfo;
use miden_client::account::{Account, AccountId};
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::auth::{
    Approver,
    ApproverSet,
    AuthMultisig,
    AuthMultisigConfig,
    AuthSchemeId,
    AuthSecretKey,
    NoAuth,
};
use miden_client::block::BlockNumber;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::rpc::domain::account::{
    AccountProof,
    AccountStorageRequirements,
    GetAccountRequest,
};
use miden_client::rpc::domain::account_vault::AccountVaultInfo;
use miden_client::rpc::domain::note::{FetchedNote, SyncNotesBlock};
use miden_client::rpc::domain::nullifier::NullifierUpdate;
use miden_client::rpc::domain::storage_map::StorageMapInfo;
use miden_client::rpc::domain::sync::{ChainMmrInfo, SyncTarget};
use miden_client::rpc::domain::transaction::TransactionRecord;
use miden_client::rpc::encryption::{AttestedTransactionEncryptionKey, SealedTransactionInputs};
use miden_client::rpc::{
    AccountStateAt,
    NetworkNoteStatusInfo,
    NodeRpcClient,
    RpcError,
    RpcLimits,
    RpcStatusInfo,
};
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::MockRpcApi;
use miden_client::transaction::{
    ForeignAccount,
    TransactionExecutorError,
    TransactionRequest,
    TransactionRequestBuilder,
};
use miden_client::{Client, ClientError, Word, async_trait};
use miden_client_sqlite_store::ClientBuilderSqliteExt;
use miden_protocol::Felt;
use miden_protocol::account::{AccountBuilder, AccountType};
use miden_protocol::address::NetworkId;
use miden_protocol::batch::{ProposedBatch, ProvenBatch};
use miden_protocol::block::{BlockHeader, SignedBlock};
use miden_protocol::crypto::SequentialCommit;
use miden_protocol::crypto::merkle::mmr::MmrProof;
use miden_protocol::crypto::rand::RandomCoin;
use miden_protocol::note::{NoteId, NoteScript, NoteTag};
use miden_protocol::testing::account_id::ACCOUNT_ID_FEE_FAUCET;
use miden_protocol::transaction::{ProvenTransaction, TransactionSummary};
use miden_protocol::vm::ExecutionProof;
use miden_standards::account::AccountBuilderSchemaCommitmentExt;
use miden_standards::account::auth::MultisigAuthArgs;
use miden_standards::account::wallets::BasicWallet;
use miden_testing::MockChainBuilder;

/// Base fee used by the protocol's own fee-payment tests. Large enough that the computed fee is
/// non-zero, so a fee note is built into the summary.
const VERIFICATION_BASE_FEE: u32 = 500;

/// Balance of the fee asset given to the paying account. `pay_fee` withdraws from the vault.
const FEE_ASSET_BALANCE: u64 = 1_000_000;

/// The client type used by these tests.
type TestClient = Client<FilesystemKeyStore>;

// PRUNING RPC WRAPPER
// ================================================================================================

/// Wraps a [`MockRpcApi`] and refuses to serve account state at a block older than a configurable
/// horizon, modelling a node that has pruned historical account state.
///
/// The rule is applied uniformly to every `get_account` request: a request pinned to a block below
/// the horizon fails, whatever the caller. That is what makes the regression test meaningful — the
/// old anchor path and the new tip path are subject to the same policy and differ only in which
/// block they ask for.
#[derive(Clone)]
struct PruningRpcApi {
    inner: MockRpcApi,
    /// The oldest block whose account state the node still serves. `None` disables pruning.
    prune_before: Arc<Mutex<Option<BlockNumber>>>,
}

impl PruningRpcApi {
    fn new(inner: MockRpcApi) -> Self {
        Self {
            inner,
            prune_before: Arc::new(Mutex::new(None)),
        }
    }

    /// Starts refusing account state pinned to a block strictly below `horizon`.
    fn prune_before(&self, horizon: BlockNumber) {
        *self.prune_before.lock().unwrap() = Some(horizon);
    }
}

#[async_trait]
impl NodeRpcClient for PruningRpcApi {
    async fn get_account(
        &self,
        account_id: AccountId,
        request: GetAccountRequest,
    ) -> Result<(BlockNumber, AccountProof), RpcError> {
        if let Some(horizon) = *self.prune_before.lock().unwrap()
            && let AccountStateAt::Block(block_num) = request.at
            && block_num < horizon
        {
            return Err(RpcError::InvalidResponse(format!(
                "account state at block {block_num} has been pruned; the node serves state only \
                 from block {horizon} onward"
            )));
        }
        self.inner.get_account(account_id, request).await
    }

    // Everything else delegates unchanged to the wrapped mock.

    async fn set_genesis_commitment(&self, commitment: Word) -> Result<(), RpcError> {
        self.inner.set_genesis_commitment(commitment).await
    }

    fn has_genesis_commitment(&self) -> Option<Word> {
        self.inner.has_genesis_commitment()
    }

    async fn get_transaction_encryption_key(
        &self,
    ) -> Result<AttestedTransactionEncryptionKey, RpcError> {
        self.inner.get_transaction_encryption_key().await
    }

    async fn submit_proven_transaction(
        &self,
        proven_transaction: &ProvenTransaction,
        sealed_transaction_inputs: SealedTransactionInputs,
    ) -> Result<BlockNumber, RpcError> {
        self.inner
            .submit_proven_transaction(proven_transaction, sealed_transaction_inputs)
            .await
    }

    async fn submit_proven_batch(
        &self,
        proven_batch: &ProvenBatch,
        proposed_batch: &ProposedBatch,
        transaction_inputs: Vec<SealedTransactionInputs>,
    ) -> Result<BlockNumber, RpcError> {
        self.inner
            .submit_proven_batch(proven_batch, proposed_batch, transaction_inputs)
            .await
    }

    async fn get_block_header_by_number(
        &self,
        block_num: Option<BlockNumber>,
        include_mmr_proof: bool,
    ) -> Result<(BlockHeader, Option<MmrProof>), RpcError> {
        self.inner.get_block_header_by_number(block_num, include_mmr_proof).await
    }

    async fn get_block_by_number(
        &self,
        block_num: BlockNumber,
        include_proof: bool,
    ) -> Result<(SignedBlock, Option<ExecutionProof>), RpcError> {
        self.inner.get_block_by_number(block_num, include_proof).await
    }

    async fn get_notes_by_id(&self, note_ids: &[NoteId]) -> Result<Vec<FetchedNote>, RpcError> {
        self.inner.get_notes_by_id(note_ids).await
    }

    async fn sync_chain_mmr(
        &self,
        current_block_height: BlockNumber,
        upper_bound: SyncTarget,
    ) -> Result<ChainMmrInfo, RpcError> {
        self.inner.sync_chain_mmr(current_block_height, upper_bound).await
    }

    async fn sync_notes(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        note_tags: &BTreeSet<NoteTag>,
    ) -> Result<Vec<SyncNotesBlock>, RpcError> {
        self.inner.sync_notes(block_from, block_to, note_tags).await
    }

    async fn sync_nullifiers(
        &self,
        prefix: &[u16],
        block_from: BlockNumber,
        block_to: BlockNumber,
    ) -> Result<Vec<NullifierUpdate>, RpcError> {
        self.inner.sync_nullifiers(prefix, block_from, block_to).await
    }

    async fn register_account(
        &self,
        invitation_code: &str,
        account_id: AccountId,
    ) -> Result<(), RpcError> {
        self.inner.register_account(invitation_code, account_id).await
    }

    async fn is_account_allowed(&self, account_id: AccountId) -> Result<bool, RpcError> {
        self.inner.is_account_allowed(account_id).await
    }

    async fn get_note_script_by_root(&self, root: Word) -> Result<Option<NoteScript>, RpcError> {
        self.inner.get_note_script_by_root(root).await
    }

    async fn sync_storage_maps(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<StorageMapInfo, RpcError> {
        self.inner.sync_storage_maps(block_from, block_to, account_id).await
    }

    async fn sync_account_vault(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_id: AccountId,
    ) -> Result<AccountVaultInfo, RpcError> {
        self.inner.sync_account_vault(block_from, block_to, account_id).await
    }

    async fn sync_transactions(
        &self,
        block_from: BlockNumber,
        block_to: BlockNumber,
        account_ids: Vec<AccountId>,
    ) -> Result<Vec<TransactionRecord>, RpcError> {
        self.inner.sync_transactions(block_from, block_to, account_ids).await
    }

    async fn get_network_id(&self) -> Result<NetworkId, RpcError> {
        self.inner.get_network_id().await
    }

    async fn get_rpc_limits(&self) -> Result<RpcLimits, RpcError> {
        self.inner.get_rpc_limits().await
    }

    fn has_rpc_limits(&self) -> Option<RpcLimits> {
        self.inner.has_rpc_limits()
    }

    async fn set_rpc_limits(&self, limits: RpcLimits) {
        self.inner.set_rpc_limits(limits).await;
    }

    async fn get_status_unversioned(&self) -> Result<RpcStatusInfo, RpcError> {
        self.inner.get_status_unversioned().await
    }

    async fn get_network_note_status(
        &self,
        note_id: NoteId,
    ) -> Result<NetworkNoteStatusInfo, RpcError> {
        self.inner.get_network_note_status(note_id).await
    }
}

// HELPERS
// ================================================================================================

/// The pieces a test drives: the client, its multisig account, the fee faucet id, the underlying
/// mock (used to prove blocks), and the pruning control.
struct MultisigFixture {
    client: TestClient,
    account: Account,
    fee_faucet_id: AccountId,
    /// A public account declared as a foreign account by the request, so execution fetches its state
    /// at the reference block — the load the node cannot serve once that block is pruned.
    foreign_account_id: AccountId,
    mock: MockRpcApi,
    pruning: PruningRpcApi,
}

/// Builds a fee-charging chain with a single-approver `AuthMultisig` account and a separate public
/// account (used as the request's foreign account), behind a pruning RPC wrapper.
async fn multisig_fixture() -> MultisigFixture {
    let fee_faucet_id: AccountId = ACCOUNT_ID_FEE_FAUCET.try_into().unwrap();
    let fee_asset: Asset = FungibleAsset::new(fee_faucet_id, FEE_ASSET_BALANCE).unwrap().into();

    let key = AuthSecretKey::new_falcon512_poseidon2();
    let approvers = ApproverSet::new(
        vec![Approver::new(
            key.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )],
        1,
    )
    .unwrap();
    let auth = AuthMultisig::new(AuthMultisigConfig::new(approvers)).unwrap();

    let mut account = AccountBuilder::new([11u8; 32])
        .account_type(AccountType::Public)
        .with_component(auth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();
    // The chain treats this account as already deployed and holding enough of the fee asset to pay.
    account.vault_mut().add_asset(fee_asset).unwrap();
    account.set_nonce(Felt::ONE).unwrap();

    // A separate public account the request declares as a foreign account. Its state is fetched from
    // the node at the execution reference block, which is what pruning at the bound block breaks.
    let mut foreign_account = AccountBuilder::new([22u8; 32])
        .account_type(AccountType::Public)
        .with_component(NoAuth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();
    foreign_account.set_nonce(Felt::ONE).unwrap();

    let mut builder = MockChainBuilder::new().verification_base_fee(VERIFICATION_BASE_FEE);
    builder.add_account(account.clone()).unwrap();
    builder.add_account(foreign_account.clone()).unwrap();
    let chain = builder.build().unwrap();
    let protocol_config = chain.protocol_config().clone();

    let mock = MockRpcApi::new(chain);
    let pruning = PruningRpcApi::new(mock.clone());

    let keystore = FilesystemKeyStore::new(temp_dir()).unwrap();
    keystore.add_key(&key, account.id()).await.unwrap();

    let mut client = ClientBuilder::new()
        .rpc(Arc::new(pruning.clone()))
        .rng(Box::new(RandomCoin::new(Word::from([0xfeeu32, 1, 2, 3]))))
        .sqlite_store(create_test_store_path())
        .authenticator(Arc::new(keystore))
        .tx_discard_delta(None)
        // Keep historical block headers so an aged proposal's bound block stays anchorable at the
        // tip. Re-deriving a multisig summary tracks the bound block in a fresh tip anchor, which
        // needs that block's header and MMR path locally.
        .irrelevant_block_prune_interval(None)
        .build()
        .await
        .unwrap();

    client.ensure_genesis_in_place().await.unwrap();
    client.seed_protocol_config(protocol_config).await.unwrap();
    client.add_account(&account, false).await.unwrap();
    client.sync_state().await.unwrap();

    MultisigFixture {
        client,
        account,
        fee_faucet_id,
        foreign_account_id: foreign_account.id(),
        mock,
        pruning,
    }
}

/// Builds a multisig request that commits `MultisigAuthArgs` binding the summary to `bound_block`,
/// paying the fee in `fee_faucet_id`'s asset at rate 1/1 under `salt`, and declaring
/// `foreign_account_id` as a public foreign account so execution fetches its state at the reference
/// block. The foreign account is loaded but unused, so it does not enter the summary.
fn multisig_request(
    bound_block: BlockNumber,
    salt: Word,
    fee_faucet_id: AccountId,
    foreign_account_id: AccountId,
) -> TransactionRequest {
    let auth_args = MultisigAuthArgs::new(bound_block, salt)
        .with_conversion_info(FeeConversionInfo::one_to_one(fee_faucet_id));
    let auth_arg = auth_args.to_commitment();
    let preimage = auth_args.to_elements();

    TransactionRequestBuilder::new()
        .auth_arg(auth_arg)
        .extend_advice_map([(auth_arg, preimage)])
        .foreign_accounts([ForeignAccount::public(
            foreign_account_id,
            AccountStorageRequirements::default(),
        )
        .unwrap()])
        .build()
        .unwrap()
}

/// Asserts a client error is the `Unauthorized` proposal outcome and returns the surfaced summary.
///
/// Reaching this outcome means execution loaded the request's foreign account and built the summary,
/// i.e. the foreign-account load succeeded.
fn expect_unauthorized_summary(err: ClientError) -> TransactionSummary {
    match err {
        ClientError::TransactionExecutorError(
            inner @ TransactionExecutorError::Unauthorized(_),
        ) => *inner.unwrap_unauthorized_err(),
        other => panic!("expected an Unauthorized proposal outcome, got: {other:?}"),
    }
}

// TESTS
// ================================================================================================

/// The core assumption behind the fix: a multisig summary binds the caller-chosen block from its
/// auth args, not the execution reference block. Re-deriving the same request at a later reference
/// block (through the new tip path) reproduces the exact commitment bound at the caller-chosen block.
#[tokio::test]
async fn multisig_summary_commitment_is_independent_of_execution_reference_block() {
    let MultisigFixture {
        mut client,
        account,
        fee_faucet_id,
        foreign_account_id,
        mock,
        ..
    } = Box::pin(multisig_fixture()).await;

    // Advance off genesis so the bound block is an ordinary, non-genesis block.
    for _ in 0..2 {
        mock.prove_block();
    }
    client.sync_state().await.unwrap();

    let bound_block = client.get_sync_height().await.unwrap();
    let salt = Word::from([7u32, 7, 7, 7]);

    // Derive the summary at the bound block itself (the reference block equals the bound block).
    let bound_summary = expect_unauthorized_summary(
        Box::pin(client.execute_transaction(
            account.id(),
            multisig_request(bound_block, salt, fee_faucet_id, foreign_account_id),
        ))
        .await
        .expect_err("a multisig request without signatures surfaces the summary as Unauthorized"),
    );
    assert_eq!(
        bound_summary.block_number(),
        bound_block,
        "the summary must bind the caller-chosen block"
    );
    // Precondition: the chain really charges, so a fee note is built into the summary. Without this
    // the test would prove nothing about the fee-paying path.
    assert_eq!(
        bound_summary.output_notes().num_notes(),
        1,
        "precondition: a fee-paying transaction builds the fee note into the summary"
    );
    let bound_commitment = bound_summary.to_commitment();

    // Advance the chain well past the bound block and sync.
    for _ in 0..5 {
        mock.prove_block();
    }
    client.sync_state().await.unwrap();
    let tip = client.get_sync_height().await.unwrap();
    assert!(tip > bound_block, "the chain must have advanced past the bound block");

    // Re-derive the same proposal at the tip. The summary stays bound to `bound_block`.
    let tip_summary = expect_unauthorized_summary(
        Box::pin(client.execute_multisig_summary_at_tip(
            account.id(),
            multisig_request(bound_block, salt, fee_faucet_id, foreign_account_id),
            bound_block,
        ))
        .await
        .expect_err("re-deriving without signatures surfaces the summary as Unauthorized"),
    );

    assert_eq!(
        tip_summary.block_number(),
        bound_block,
        "re-derivation at the tip must still bind the caller-chosen block"
    );
    assert_eq!(
        bound_commitment,
        tip_summary.to_commitment(),
        "the summary commitment must be identical whether derived at the bound block or the tip"
    );
}

/// The regression this fix is about: once the node prunes account state at the proposal's anchor
/// block, re-deriving against that anchor fails while re-deriving at the tip succeeds (reaches the
/// summary) and reproduces the same commitment.
#[tokio::test]
async fn multisig_summary_tip_execution_survives_a_pruned_anchor() {
    let MultisigFixture {
        mut client,
        account,
        fee_faucet_id,
        foreign_account_id,
        mock,
        pruning,
    } = Box::pin(multisig_fixture()).await;

    // Advance off genesis so the bound block is an ordinary, non-genesis block.
    for _ in 0..2 {
        mock.prove_block();
    }
    client.sync_state().await.unwrap();

    let bound_block = client.get_sync_height().await.unwrap();
    let salt = Word::from([9u32, 9, 9, 9]);

    // Capture the baseline commitment and the proposer's anchor at the bound block, before pruning.
    let baseline_commitment = expect_unauthorized_summary(
        Box::pin(client.execute_transaction(
            account.id(),
            multisig_request(bound_block, salt, fee_faucet_id, foreign_account_id),
        ))
        .await
        .expect_err("the baseline proposal surfaces the summary as Unauthorized"),
    )
    .to_commitment();
    let stale_anchor = client
        .chain_anchor_for_request(&multisig_request(
            bound_block,
            salt,
            fee_faucet_id,
            foreign_account_id,
        ))
        .await
        .unwrap();
    assert_eq!(stale_anchor.block_num(), bound_block);

    // Advance the chain past the anchor and sync.
    for _ in 0..5 {
        mock.prove_block();
    }
    client.sync_state().await.unwrap();
    let tip = client.get_sync_height().await.unwrap();
    assert!(tip > bound_block);

    // The node now prunes account state at every block up to and including the bound block.
    pruning.prune_before(bound_block.child());

    // OLD PATH: executing against the stale anchor fetches the foreign account at the pruned bound
    // block and fails while loading it, before the summary can be built.
    let old_path_err = Box::pin(client.execute_transaction_at(
        account.id(),
        multisig_request(bound_block, salt, fee_faucet_id, foreign_account_id),
        stale_anchor,
    ))
    .await
    .expect_err("the stale-anchor path must fail once the anchor block is pruned");
    assert!(
        !matches!(
            old_path_err,
            ClientError::TransactionExecutorError(TransactionExecutorError::Unauthorized(_))
        ),
        "the stale-anchor path must fail before reaching the summary, not surface one"
    );
    let rendered = format!("{old_path_err:?}");
    assert!(
        rendered.contains("pruned"),
        "the failure must be the pruned foreign-account load, got: {rendered}"
    );

    // NEW PATH: re-deriving at the tip fetches the foreign account at a served block, reaches the
    // summary, and reproduces the commitment bound at the bound block.
    let tip_summary = expect_unauthorized_summary(
        Box::pin(client.execute_multisig_summary_at_tip(
            account.id(),
            multisig_request(bound_block, salt, fee_faucet_id, foreign_account_id),
            bound_block,
        ))
        .await
        .expect_err("the tip path reaches the summary and surfaces it as Unauthorized"),
    );

    assert_eq!(
        tip_summary.block_number(),
        bound_block,
        "the tip path must keep the summary bound to the caller-chosen block"
    );
    assert_eq!(
        baseline_commitment,
        tip_summary.to_commitment(),
        "the tip path must reproduce the same summary commitment"
    );
}

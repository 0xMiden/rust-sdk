//! Guardian execution against a local stack.
//!
//! The test needs a test node (`make start-node-background`) and a Guardian server with a
//! configured prover. It reads `TEST_GUARDIAN_URL`, `MIDEN_FUNDER_ACCOUNTS_DIR` and
//! `TEST_MIDEN_PROVER_URL`. A local proof in a debug build takes longer than the expiration delta
//! of the funding transaction, so a debug run needs the remote prover.

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use miden_client::account::standards::auth::{
    Approver,
    ApproverSet,
    AuthGuardedMultisig,
    AuthGuardedMultisigConfig,
    FeeConversionInfo,
    GuardianConfig,
    MultisigAuthArgs,
};
use miden_client::account::standards::wallets::BasicWallet;
use miden_client::account::{
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountFile,
    AccountId,
    AccountType,
};
use miden_client::asset::{Asset, AssetId, FungibleAsset};
use miden_client::auth::{AuthSchemeId, AuthSecretKey};
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::note::{BlockNumber, Note, NoteType, P2idNote};
use miden_client::rpc::{Endpoint, GrpcClient};
use miden_client::store::NoteFilter;
use miden_client::transaction::{InputNote, TransactionRequestBuilder};
use miden_client::{Client, RemoteTransactionProver, Word};
use miden_client_sqlite_store::SqliteStore;
use miden_guarded_client::GuardedClient;

const GUARDIAN_URL_ENV: &str = "TEST_GUARDIAN_URL";
/// The directory where the test node writes its funder wallets at genesis.
const FUNDER_ACCOUNTS_ENV: &str = "MIDEN_FUNDER_ACCOUNTS_DIR";
const DEFAULT_RPC_TIMEOUT_MS: u64 = 10_000;
const FUNDING_AMOUNT: u64 = 10_000_000;
/// Guardian refuses a proposal whose approval does not expire.
const APPROVAL_EXPIRATION_DELTA: u32 = 1_000;
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(300);

/// Two clients share a private guarded multisig with a threshold of 1. Client 1 registers the
/// account in Guardian and proposes a transaction that consumes a funding note. Client 2 imports
/// the account from Guardian and asks Guardian to execute the proposal. Both clients then sync and
/// must hold the same account state and see the note as consumed.
#[tokio::test]
#[ignore = "needs a local test node and a Guardian server"]
#[allow(clippy::too_many_lines)]
async fn guardian_executes_a_signed_proposal() {
    // Create two clients, one for each cosigner
    let dir_1 = tempfile::tempdir().unwrap();
    let dir_2 = tempfile::tempdir().unwrap();
    let (mut client_1, keystore_1) = new_client(dir_1.path()).await;
    let (mut client_2, keystore_2) = new_client(dir_2.path()).await;
    client_1.sync_state().await.unwrap();
    client_2.sync_state().await.unwrap();

    // Set up the guarded multisig
    let guardian_url = guardian_url();
    let guardian_key = GuardedClient::<FilesystemKeyStore>::guardian_key(guardian_url.clone())
        .await
        .unwrap();
    let cosigner_key_1 = AuthSecretKey::new_falcon512_poseidon2();
    let cosigner_key_2 = AuthSecretKey::new_falcon512_poseidon2();
    let approvers = ApproverSet::new(
        vec![
            Approver::new(
                cosigner_key_1.public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            ),
            Approver::new(
                cosigner_key_2.public_key().to_commitment(),
                AuthSchemeId::Falcon512Poseidon2,
            ),
        ],
        1,
    )
    .unwrap();
    let guardian_config =
        GuardianConfig::new(Approver::new(guardian_key, AuthSchemeId::Falcon512Poseidon2));
    let auth = AuthGuardedMultisig::new(
        AuthGuardedMultisigConfig::new(approvers, guardian_config).unwrap(),
    )
    .unwrap();
    let account = AccountBuilder::new(rand::random())
        .account_type(AccountType::Private)
        .with_component(auth)
        .with_component(BasicWallet)
        .build_with_schema_commitment()
        .unwrap();

    keystore_1.add_key(&cosigner_key_1, account.id()).await.unwrap();
    keystore_2.add_key(&cosigner_key_2, account.id()).await.unwrap();
    client_1.add_account(&account, false).await.unwrap();

    // Fund the account
    let funding_note = fund_account(&mut client_1, &keystore_1, account.id()).await;

    // Register the account in Guardian
    let mut guarded_1 = GuardedClient::new(
        client_1,
        account.id(),
        guardian_url.clone(),
        cosigner_key_1.public_key().to_commitment(),
    )
    .await
    .unwrap();
    guarded_1.register().await.unwrap();

    // Propose a transaction that consumes the funding note
    let fee_faucet_id = funding_note
        .assets()
        .iter()
        .find_map(Asset::as_fungible)
        .expect("the funding note carries the native fee asset")
        .faucet_id();

    // Explicit auth args and input notes, so that Guardian computes the same summary
    let bound_block = guarded_1.sync_height().await.unwrap();
    let auth_args = MultisigAuthArgs::new(bound_block, Word::from(rand::random::<[u32; 4]>()))
        .with_approval_expiration_delta(NonZeroU32::new(APPROVAL_EXPIRATION_DELTA).unwrap())
        .unwrap()
        .with_conversion_info(FeeConversionInfo::one_to_one(fee_faucet_id));
    let request = TransactionRequestBuilder::new()
        .explicit_input_notes([(InputNote::unauthenticated(funding_note.clone()), None)])
        .auth_args(&auth_args)
        .build()
        .unwrap();

    let summary = guarded_1.propose(request).await.unwrap();

    // Import the account and the proposal in client 2
    let mut guarded_2 = GuardedClient::import(
        client_2,
        account.id(),
        guardian_url,
        cosigner_key_2.public_key().to_commitment(),
    )
    .await
    .unwrap();
    assert_eq!(guarded_2.account().await.unwrap().to_commitment(), account.to_commitment());

    let summary_2 = guarded_2.proposal_summary(summary.to_commitment()).await.unwrap();

    // Delegate submission to Guardian
    let execution = guarded_2.request_guardian_execution(&summary_2).await.unwrap();
    guarded_2
        .wait_for_execution(execution.proposal_id, EXECUTION_TIMEOUT)
        .await
        .unwrap();

    // Sync client 1 and check the account and the funding note
    guarded_1.sync().await.unwrap();

    let account_1 = guarded_1.account().await.unwrap();
    let fee_asset = FungibleAsset::new(fee_faucet_id, FUNDING_AMOUNT).unwrap();
    let balance = u64::from(account_1.vault().get_balance(AssetId::from(fee_asset)).unwrap());
    assert_eq!(account_1.nonce().as_canonical_u64(), 1);
    assert!(
        balance > 0 && balance < FUNDING_AMOUNT,
        "the balance is the funds minus the fee"
    );

    let funding_record = guarded_1
        .get_input_notes(NoteFilter::List(vec![funding_note.id()]))
        .await
        .unwrap()
        .pop()
        .expect("client 1 tracks the funding note");
    assert!(funding_record.is_consumed());

    // Sync client 2 and check that it holds the same state
    guarded_2.sync().await.unwrap();

    let account_2 = guarded_2.account().await.unwrap();
    assert_eq!(account_2.to_commitment(), account_1.to_commitment());

    let funding_record_2 = guarded_2
        .get_input_notes(NoteFilter::List(vec![funding_note.id()]))
        .await
        .unwrap()
        .pop()
        .expect("client 2 tracks the funding note");
    assert!(funding_record_2.is_consumed());
}

// HELPERS
// ================================================================================================

/// Creates a client with its own store and keystore in `dir`.
///
/// The endpoints come from the same environment variables as the integration tests:
/// `TEST_MIDEN_RPC_URL` (localhost by default), `TEST_MIDEN_PROVER_URL` (local prover by default)
/// and `MIDEN_TEST_TIMEOUT`.
async fn new_client(dir: &Path) -> (Client<FilesystemKeyStore>, Arc<FilesystemKeyStore>) {
    let store = SqliteStore::new(dir.join("store.sqlite3")).await.unwrap();
    let keystore = Arc::new(FilesystemKeyStore::new(dir.join("keystore")).unwrap());

    let endpoint = std::env::var("TEST_MIDEN_RPC_URL")
        .map_or_else(|_| Endpoint::localhost(), |url| Endpoint::try_from(url.as_str()).unwrap());
    let timeout_ms = std::env::var("MIDEN_TEST_TIMEOUT")
        .ok()
        .and_then(|timeout| timeout.parse().ok())
        .unwrap_or(DEFAULT_RPC_TIMEOUT_MS);

    let mut builder = ClientBuilder::new()
        .rpc(Arc::new(GrpcClient::new(&endpoint, timeout_ms)))
        .store(Arc::new(store))
        .authenticator(keystore.clone());
    if let Ok(prover_url) = std::env::var("TEST_MIDEN_PROVER_URL") {
        builder = builder.prover(Arc::new(RemoteTransactionProver::new(prover_url)));
    }

    (builder.build().await.unwrap(), keystore)
}

/// Returns the Guardian gRPC endpoint from [`GUARDIAN_URL_ENV`].
fn guardian_url() -> String {
    std::env::var(GUARDIAN_URL_ENV).unwrap_or_else(|_| panic!("{GUARDIAN_URL_ENV} is not set"))
}

/// Sends `account_id` a P2ID note with the native fee asset from a funder wallet of the test node,
/// and returns the note. The note is not on the chain yet when the function returns.
///
/// The wallet comes from [`FUNDER_ACCOUNTS_ENV`], as in the integration tests.
async fn fund_account(
    client: &mut Client<FilesystemKeyStore>,
    keystore: &FilesystemKeyStore,
    account_id: AccountId,
) -> Note {
    let funders_dir = std::env::var(FUNDER_ACCOUNTS_ENV)
        .unwrap_or_else(|_| panic!("{FUNDER_ACCOUNTS_ENV} is not set"));
    let funder_bytes = std::fs::read(Path::new(&funders_dir).join("wallet_0.mac")).unwrap();
    let funder = AccountFile::try_from_bytes(&funder_bytes).unwrap();
    let funder_id = funder.account().id();
    for key in funder.auth_secret_keys() {
        keystore.add_key(key, funder_id).await.unwrap();
    }
    client.import_account_by_id(funder_id).await.unwrap();

    let (genesis, _) = client.get_block_header_by_num(BlockNumber::GENESIS).await.unwrap().unwrap();
    let fee_faucet_id = client
        .get_protocol_config(genesis.protocol_config_commitment())
        .await
        .unwrap()
        .fee_asset_id()
        .faucet_id();

    let note: Note = P2idNote::builder()
        .sender(funder_id)
        .target(account_id)
        .asset(FungibleAsset::new(fee_faucet_id, FUNDING_AMOUNT).unwrap())
        .note_type(NoteType::Public)
        .generate_serial_number(client.rng())
        .build()
        .unwrap()
        .into();

    let request = TransactionRequestBuilder::new()
        .own_output_notes([note.clone()])
        .build()
        .unwrap();
    Box::pin(client.submit_new_transaction(funder_id, request)).await.unwrap();

    note
}

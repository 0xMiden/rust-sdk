//! Measures local transaction execution and note screening through the client, with a `SqliteStore`
//! and a mock node.
//!
//! The file only uses client APIs that exist on `next` and on the code registry branches, so it can
//! be copied to either and run with the same command:
//!
//! `cargo test --release -p miden-client-unit-tests --test mast_store_bench -- --ignored
//! --nocapture`

use std::env::temp_dir;
use std::sync::Arc;
use std::time::{Duration, Instant};

use miden_client::Client;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::FilesystemKeyStore;
use miden_client::note::BlockNumber;
use miden_client::rpc::NodeRpcClient;
use miden_client::rpc::encryption::TransactionEncryptionKey;
use miden_client::store::Store;
use miden_client::testing::common::create_test_store_path;
use miden_client::testing::mock::MockRpcApi;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client_sqlite_store::SqliteStore;
use miden_protocol::account::AccountId;
use miden_protocol::asset::{Asset, FungibleAsset};
use miden_protocol::crypto::dsa::eddsa_25519_sha512::KeyExchangeKey;
use miden_protocol::note::{Note, NoteType};
use miden_protocol::testing::account_id::{
    ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET,
    ACCOUNT_ID_PRIVATE_SENDER,
};
use miden_testing::{Auth, MockChainBuilder};
use rand::SeedableRng;
use rand::rngs::StdRng;

/// The number of notes of each kind.
const NUM_NOTES: u32 = 100;
/// The number of runs that the result of a scenario with many executions per run is the mean of.
const NUM_RUNS: u32 = 5;
/// The number of runs that the result of a scenario with one execution per run is the mean of.
const NUM_SINGLE_RUNS: u32 = 20;

// SETUP
// ================================================================================================

/// A client that tracks a wallet, and notes that the wallet can consume.
struct Setup {
    client: Client<FilesystemKeyStore>,
    account_id: AccountId,
    /// P2ID notes to the wallet.
    p2id_notes: Vec<Note>,
    /// P2ANY notes. They are not standard notes, so screening executes them.
    p2any_notes: Vec<Note>,
    /// The time to build the client.
    build_time: Duration,
}

async fn setup() -> Setup {
    let sender_id: AccountId = ACCOUNT_ID_PRIVATE_SENDER.try_into().unwrap();
    let faucet_id: AccountId = ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET.try_into().unwrap();
    let asset = Asset::from(FungibleAsset::new(faucet_id, 10).unwrap());

    let mut builder = MockChainBuilder::new();
    let wallet = builder.add_existing_wallet(Auth::IncrNonce).unwrap();
    let p2id_notes = (0..NUM_NOTES)
        .map(|_| {
            builder
                .add_p2id_note(sender_id, wallet.id(), &[asset], NoteType::Public)
                .unwrap()
        })
        .collect();
    let p2any_notes = (0..NUM_NOTES)
        .map(|_| builder.add_p2any_note(sender_id, NoteType::Public, [asset]).unwrap())
        .collect();
    let mut chain = builder.build().unwrap();
    chain.prove_next_block().unwrap();

    let client_builder = mock_client_builder(Arc::new(MockRpcApi::new(chain))).await;
    let start = Instant::now();
    let mut client = client_builder.build().await.unwrap();
    let build_time = start.elapsed();
    client.add_account(&wallet, false).await.unwrap();

    Setup {
        client,
        account_id: wallet.id(),
        p2id_notes,
        p2any_notes,
        build_time,
    }
}

/// Returns a builder for a client backed by `rpc_api` and a new SQLite store that holds the genesis
/// header, the protocol configuration and a transaction encryption key.
async fn mock_client_builder(rpc_api: Arc<MockRpcApi>) -> ClientBuilder<FilesystemKeyStore> {
    let store = SqliteStore::new(create_test_store_path()).await.unwrap();

    let (genesis, _) = rpc_api
        .get_block_header_by_number(Some(BlockNumber::GENESIS), false)
        .await
        .unwrap();
    store.insert_block_header(&genesis, &[], false).await.unwrap();
    store.insert_protocol_config(&rpc_api.protocol_config()).await.unwrap();
    store
        .set_transaction_encryption_key(&TransactionEncryptionKey::new_unattested(
            b"mock-key-id".to_vec(),
            KeyExchangeKey::with_rng(&mut StdRng::seed_from_u64(0xface)).public_key(),
            genesis.commitment(),
        ))
        .await
        .unwrap();

    ClientBuilder::new()
        .rpc(rpc_api)
        .store(Arc::new(store))
        .authenticator(Arc::new(FilesystemKeyStore::new(temp_dir()).unwrap()))
        .tx_discard_delta(None)
}

// SCENARIOS
// ================================================================================================

/// Executes a transaction that consumes `notes`, without proving or submitting it.
async fn execute(setup: &Setup, notes: &[Note]) {
    let request = TransactionRequestBuilder::new()
        .input_notes(notes.iter().map(|note| (note.clone(), None)))
        .build()
        .unwrap();
    Box::pin(setup.client.execute_transaction(setup.account_id, request))
        .await
        .expect("the transaction executes");
}

/// Checks whether the wallet can consume each note, with one new note screener.
async fn screen(setup: &Setup, notes: &[Note]) {
    let consumability = setup
        .client
        .note_screener()
        .get_batch_consumability_for_account(setup.account_id, notes)
        .await
        .unwrap();
    assert_eq!(consumability.len(), notes.len());
}

/// Runs `scenario` `runs` times and returns the mean time of one run.
async fn measure<F, Fut>(runs: u32, mut scenario: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut total = Duration::ZERO;
    for _ in 0..runs {
        let start = Instant::now();
        scenario().await;
        total += start.elapsed();
    }
    total / runs
}

// BENCHMARK
// ================================================================================================

#[tokio::test]
#[ignore = "benchmark: run with --ignored --nocapture"]
async fn mast_store_bench() {
    let setup = setup().await;
    println!("release build, {NUM_NOTES} notes");
    println!("0. build the client: {:?}", setup.build_time);

    let time = measure(NUM_RUNS, || async {
        for note in &setup.p2id_notes {
            execute(&setup, core::slice::from_ref(note)).await;
        }
    })
    .await;
    println!(
        "1. {NUM_NOTES} transactions with 1 P2ID note each, mean of {NUM_RUNS} runs: {time:?} ({:?} \
         per transaction)",
        time / NUM_NOTES
    );

    let time = measure(NUM_SINGLE_RUNS, || execute(&setup, &setup.p2id_notes)).await;
    println!(
        "2. 1 transaction with {NUM_NOTES} P2ID notes, mean of {NUM_SINGLE_RUNS} runs: {time:?}"
    );

    let time = measure(NUM_RUNS, || screen(&setup, &setup.p2any_notes)).await;
    println!(
        "3. screen {NUM_NOTES} P2ANY notes, mean of {NUM_RUNS} runs: {time:?} ({:?} per note)",
        time / NUM_NOTES
    );

    let first_note = core::slice::from_ref(setup.p2any_notes.first().unwrap());
    let time = measure(NUM_SINGLE_RUNS, || screen(&setup, first_note)).await;
    println!(
        "4. screen 1 P2ANY note with a new screener, mean of {NUM_SINGLE_RUNS} runs: {time:?}"
    );
}

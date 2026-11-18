//! Compares local transaction execution with the in-memory MAST store and with [`SqliteStore`].
//!
//! Run with: `cargo test --release -p miden-client-sqlite-store --lib code::bench -- --ignored
//! --nocapture`

use std::collections::BTreeSet;
use std::println;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use std::vec::Vec;

use miden_protocol::account::{AccountId, PartialAccount, StorageMapKey, StorageMapWitness};
use miden_protocol::asset::{AssetId, AssetWitness, FungibleAsset};
use miden_protocol::block::{BlockHeader, BlockNumber};
use miden_protocol::note::{Note, NoteScript, NoteScriptRoot, NoteType};
use miden_protocol::protocol_config::ProtocolConfig;
use miden_protocol::testing::account_id::ACCOUNT_ID_SENDER;
use miden_protocol::transaction::{AccountInputs, PartialBlockchain, TransactionKernel};
use miden_protocol::vm::FutureMaybeSend;
use miden_protocol::{CoreLibrary, MastForest, ProtocolLib, Word};
use miden_standards::StandardsLib;
use miden_testing::{Auth, MockChain, MockTransaction};
use miden_tx::auth::UnreachableAuth;
use miden_tx::{
    DataStore,
    DataStoreError,
    LoadedMastForest,
    MastForestStore,
    TransactionExecutor,
    TransactionMastStore,
};

use crate::tests::create_test_store;

const NUM_NOTES: usize = 100;
const NUM_RUNS: u32 = 10;

// DATA STORE
// ================================================================================================

/// Serves the transaction data from a mock transaction and the MAST forests from `mast_store`.
struct BenchDataStore<'a, M> {
    tx: &'a MockTransaction,
    mast_store: &'a M,
    /// The number of MAST forest requests during execution.
    gets: AtomicUsize,
}

impl<M: MastForestStore> MastForestStore for BenchDataStore<'_, M> {
    fn get(&self, procedure_hash: &Word) -> Option<LoadedMastForest> {
        self.gets.fetch_add(1, Ordering::Relaxed);
        self.mast_store.get(procedure_hash)
    }
}

impl<M: MastForestStore + Sync> DataStore for BenchDataStore<'_, M> {
    fn get_transaction_inputs(
        &self,
        account_id: AccountId,
        ref_blocks: BTreeSet<BlockNumber>,
    ) -> impl FutureMaybeSend<
        Result<(PartialAccount, BlockHeader, ProtocolConfig, PartialBlockchain), DataStoreError>,
    > {
        self.tx.get_transaction_inputs(account_id, ref_blocks)
    }

    fn get_foreign_account_inputs(
        &self,
        foreign_account_id: AccountId,
        ref_block: BlockNumber,
    ) -> impl FutureMaybeSend<Result<AccountInputs, DataStoreError>> {
        self.tx.get_foreign_account_inputs(foreign_account_id, ref_block)
    }

    fn get_vault_asset_witnesses(
        &self,
        account_id: AccountId,
        vault_root: Word,
        asset_ids: BTreeSet<AssetId>,
    ) -> impl FutureMaybeSend<Result<Vec<AssetWitness>, DataStoreError>> {
        self.tx.get_vault_asset_witnesses(account_id, vault_root, asset_ids)
    }

    fn get_storage_map_witness(
        &self,
        account_id: AccountId,
        map_root: Word,
        map_key: StorageMapKey,
    ) -> impl FutureMaybeSend<Result<StorageMapWitness, DataStoreError>> {
        self.tx.get_storage_map_witness(account_id, map_root, map_key)
    }

    fn get_note_script(
        &self,
        script_root: NoteScriptRoot,
    ) -> impl FutureMaybeSend<Result<Option<NoteScript>, DataStoreError>> {
        self.tx.get_note_script(script_root)
    }
}

// BENCHMARK
// ================================================================================================

/// Executes `tx` `NUM_RUNS` times with `mast_store`, and returns the mean execution time and the
/// number of MAST forest requests in one execution.
async fn execute<M: MastForestStore + Sync>(
    tx: &MockTransaction,
    mast_store: &M,
) -> (Duration, usize) {
    let data_store = BenchDataStore {
        tx,
        mast_store,
        gets: AtomicUsize::new(0),
    };
    let executor = TransactionExecutor::<'_, '_, _, UnreachableAuth>::new(&data_store);

    let mut total = Duration::ZERO;
    for _ in 0..NUM_RUNS {
        data_store.gets.store(0, Ordering::Relaxed);
        let start = Instant::now();
        executor
            .execute_transaction(
                tx.account().id(),
                tx.tx_inputs().block_header().block_num(),
                tx.input_notes().clone(),
                tx.tx_args().clone(),
            )
            .await
            .expect("the transaction executes");
        total += start.elapsed();
    }

    (total / NUM_RUNS, data_store.gets.load(Ordering::Relaxed))
}

#[tokio::test]
#[ignore = "benchmark: run with --ignored --nocapture"]
async fn consume_p2id_notes_with_memory_and_sqlite_mast_stores() -> anyhow::Result<()> {
    let mut builder = MockChain::builder();
    let account = builder.add_existing_wallet(Auth::IncrNonce)?;
    let sender = AccountId::try_from(ACCOUNT_ID_SENDER)?;
    let notes = (0..NUM_NOTES)
        .map(|_| {
            builder.add_p2id_note(
                sender,
                account.id(),
                &[FungibleAsset::mock(10)],
                NoteType::Public,
            )
        })
        .collect::<Result<Vec<Note>, _>>()?;
    let chain = builder.build()?;
    let tx = chain
        .build_transaction(account.id())
        .authenticated_input_notes(notes.iter().map(Note::id))
        .build()?;

    // The same MAST forests as the in-memory store, except the agglayer library, which P2ID notes
    // do not use.
    let forests: Vec<Arc<MastForest>> = vec![
        TransactionKernel::package().mast_forest().clone(),
        CoreLibrary::default().package().mast_forest().clone(),
        ProtocolLib::default().mast_forest().clone(),
        StandardsLib::default().mast_forest().clone(),
        account.code().mast(),
    ];

    let memory_store = TransactionMastStore::new();
    memory_store.load_account_code(account.code());

    let sqlite_store = create_test_store().await;
    let start = Instant::now();
    sqlite_store.add_procedures(forests).await?;
    let add_time = start.elapsed();

    let (memory_time, memory_gets) = execute(&tx, &memory_store).await;
    let (sqlite_time, sqlite_gets) = execute(&tx, &sqlite_store).await;

    println!("consume {NUM_NOTES} P2ID notes, mean of {NUM_RUNS} runs:");
    println!("  memory MAST store: {memory_time:?} ({memory_gets} MAST forest requests)");
    println!("  SQLite MAST store: {sqlite_time:?} ({sqlite_gets} MAST forest requests)");
    println!("  add_procedures for all forests: {add_time:?}");

    Ok(())
}

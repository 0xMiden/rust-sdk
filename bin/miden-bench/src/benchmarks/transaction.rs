use std::time::{Duration, Instant};

use miden_client::account::{AccountId, StorageMapKey, StorageSlotContent};
use miden_client::keystore::FilesystemKeyStore;
use miden_client::{Client, Serializable};

use crate::config::{self, BenchConfig};
use crate::deploy::wait_for_block_advancement;
use crate::masm::build_chunk_tx_request;
use crate::metrics::{BenchmarkResult, measure_time_async};
use crate::report::format_size;

// DATA MODEL
// ================================================================================================

/// Information about a bench map storage slot extracted from the account.
#[derive(Clone, Debug)]
pub struct StorageSlotInfo {
    pub name: String,
    pub keys: Vec<StorageMapKey>,
}

impl StorageSlotInfo {
    fn num_reads(&self) -> usize {
        self.keys.len()
    }
}

/// A single map entry read operation to be performed in a transaction.
#[derive(Clone, Debug)]
pub struct ReadOp {
    /// Index into the `slot_infos` array (matches the reader procedure index).
    pub slot_idx: usize,
    pub key: StorageMapKey,
}

// ORCHESTRATOR
// ================================================================================================

/// Runs transaction benchmarks (requires a running node).
///
/// The benchmark uses the specified account as the native account executing transactions.
/// Each transaction reads storage entries (both value and map slots) from the account's
/// own storage. Slot types and entries are auto-detected from the imported account storage.
///
/// The signing key is expected to be present in the persistent keystore (written by the
/// `deploy` command). When the key is available, all benchmarks (execution, proving, full)
/// are run. Otherwise only execution is benchmarked.
///
/// When `max_reads_per_tx` is provided and total reads exceed that limit, reads are
/// split across multiple transactions per benchmark iteration. Each iteration's reported
/// time is the sum across all transactions.
pub async fn run_transaction_benchmarks(
    client: &mut Client<FilesystemKeyStore>,
    config: &BenchConfig,
    account_id_str: String,
    max_reads_per_tx: Option<usize>,
) -> anyhow::Result<Vec<BenchmarkResult>> {
    let mut results = Vec::new();

    // Parse the account ID
    let account_id = AccountId::from_hex(&account_id_str)?;
    println!("Account ID: {account_id}");
    println!();

    // Import the public account from the network (skip if already present in store)
    let has_account = client.account_reader(account_id).header().await.is_ok();
    if has_account {
        println!("Using account {account_id} from persistent store");
    } else {
        println!("Importing account {account_id}...");
        client.import_account_by_id(account_id).await?;
    }

    // Detect storage slots and build slot info.
    let storage = client.get_account_storage(account_id).await?;
    let slot_infos: Vec<StorageSlotInfo> = build_slot_infos_from_storage(&storage);

    let total_reads: usize = slot_infos.iter().map(StorageSlotInfo::num_reads).sum();
    if total_reads == 0 {
        anyhow::bail!("Account has no non-empty storage slots to benchmark.");
    }

    let entries_per_map: Vec<usize> = slot_infos.iter().map(|s| s.keys.len()).collect();

    // Flatten into individual read operations and chunk
    let all_ops = flatten_read_ops(&slot_infos);
    let chunks = chunk_read_ops(&all_ops, max_reads_per_tx.unwrap_or(total_reads));
    let num_chunks = chunks.len();

    // Build a slot summary that shows per-map entry counts
    let num_maps = entries_per_map.len();
    let slot_summary = if entries_per_map.windows(2).all(|w| w[0] == w[1]) {
        format!("{num_maps} map ({} entries each)", entries_per_map[0])
    } else {
        let counts = entries_per_map.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
        format!("{num_maps} map (entries: [{counts}])")
    };

    if num_chunks > 1 {
        let reads_per_tx = max_reads_per_tx.unwrap_or(total_reads);
        println!(
            "Slots: {slot_summary}. \
             Total reads: {total_reads} ({num_chunks} txs, up to {reads_per_tx} each)"
        );
    } else {
        println!("Slots: {slot_summary}. Total reads: {total_reads}");
    }

    // Measure proven transaction size upfront (execute + prove one tx).
    // If the signing key is missing from the keystore, proving will fail and we
    // fall back to execution-only benchmarks.
    let can_prove = {
        let tx_request = build_chunk_tx_request(client, &chunks[0], &slot_infos)?;
        let tx_result = client.execute_transaction(account_id, tx_request).await?;
        if let Ok(proven_tx) = client.prove_transaction(&tx_result).await {
            let tx_size = proven_tx.to_bytes().len();
            println!("Proven transaction size: {}", format_size(tx_size));
            true
        } else {
            println!(
                "Signing key not found in keystore. \
                 Only execution benchmarks will run."
            );
            println!(
                "Hint: run `deploy` first to persist the signing key, or use the same --store path."
            );
            false
        }
    };
    println!();

    // Benchmark 1: Transaction execution time (without proving)
    println!("Benchmarking transaction execution...");
    let execution_result = benchmark_tx_execution(config, account_id, &chunks, &slot_infos).await?;
    results.push(execution_result);

    if can_prove {
        // Benchmark 2: Transaction proving time
        println!("Benchmarking transaction proving...");
        let proving_result = benchmark_tx_proving(config, account_id, &chunks, &slot_infos).await?;
        results.push(proving_result);

        // Benchmark 3: Full transaction (execute + prove + submit)
        println!("Benchmarking full transaction...");
        let full_result =
            Box::pin(benchmark_tx_full(config, account_id, &chunks, &slot_infos)).await?;
        results.push(full_result);
    }

    Ok(results)
}

// BENCHMARKS
// ================================================================================================

/// Benchmarks transaction execution time (reading storage from active account).
///
/// When multiple chunks are provided, each iteration executes all chunks sequentially
/// and reports the total execution time.
async fn benchmark_tx_execution(
    config: &BenchConfig,
    account_id: AccountId,
    chunks: &[Vec<ReadOp>],
    slot_infos: &[StorageSlotInfo],
) -> anyhow::Result<BenchmarkResult> {
    let total_reads: usize = chunks.iter().map(Vec::len).sum();
    let num_chunks = chunks.len();

    let mut result = BenchmarkResult::new(bench_name("execute", total_reads, num_chunks));

    for i in 0..config.iterations {
        let iter_t = Instant::now();

        let mut client = create_iteration_client(config).await?;
        client.sync_state().await?;

        let mut total_duration = Duration::ZERO;

        for chunk in chunks {
            let tx_request = build_chunk_tx_request(&client, chunk, slot_infos)?;

            let (_, duration) = measure_time_async(|| async {
                client.execute_transaction(account_id, tx_request).await
            })
            .await;

            total_duration += duration;
        }

        result.add_iteration(total_duration);
        println!(
            "  Iteration {}/{}: {:.2?} (total: {:.2?})",
            i + 1,
            config.iterations,
            total_duration,
            iter_t.elapsed()
        );
    }

    result = result.with_metadata(format!(
        "Transaction execution (no proving), {total_reads} storage reads from active account{}",
        if num_chunks > 1 {
            format!(" across {num_chunks} transactions")
        } else {
            String::new()
        }
    ));

    Ok(result)
}

/// Benchmarks transaction proving time.
///
/// When multiple chunks are provided, each iteration executes and proves all chunks
/// sequentially, reporting the total proving time (execution time is excluded).
async fn benchmark_tx_proving(
    config: &BenchConfig,
    account_id: AccountId,
    chunks: &[Vec<ReadOp>],
    slot_infos: &[StorageSlotInfo],
) -> anyhow::Result<BenchmarkResult> {
    let total_reads: usize = chunks.iter().map(Vec::len).sum();
    let num_chunks = chunks.len();

    let mut result = BenchmarkResult::new(bench_name("prove", total_reads, num_chunks));

    for i in 0..config.iterations {
        let iter_t = Instant::now();

        let mut client = create_iteration_client(config).await?;
        client.sync_state().await?;

        let mut total_duration = Duration::ZERO;

        for chunk in chunks {
            let tx_request = build_chunk_tx_request(&client, chunk, slot_infos)?;

            // Execute first (not measured)
            let tx_result = client.execute_transaction(account_id, tx_request).await?;

            // Measure proving time only
            let (proven_tx, duration) =
                measure_time_async(|| async { client.prove_transaction(&tx_result).await }).await;

            total_duration += duration;

            if let Ok(proven) = proven_tx {
                let proof_bytes = proven.proof().to_bytes();
                result = result.with_output_size(proof_bytes.len());
            }
        }

        result.add_iteration(total_duration);
        println!(
            "  Iteration {}/{}: {:.2?} (total: {:.2?})",
            i + 1,
            config.iterations,
            total_duration,
            iter_t.elapsed()
        );
    }

    result = result.with_metadata(format!(
        "Transaction proving, {total_reads} storage reads from active account{}",
        if num_chunks > 1 {
            format!(" across {num_chunks} transactions")
        } else {
            String::new()
        }
    ));

    Ok(result)
}

/// Benchmarks full transaction (execute + prove + submit).
///
/// When multiple chunks are provided, each iteration submits all chunks sequentially
/// with block advancement waits between submissions (needed for nonce updates).
/// Reports the total time including waits.
async fn benchmark_tx_full(
    config: &BenchConfig,
    account_id: AccountId,
    chunks: &[Vec<ReadOp>],
    slot_infos: &[StorageSlotInfo],
) -> anyhow::Result<BenchmarkResult> {
    let total_reads: usize = chunks.iter().map(Vec::len).sum();
    let num_chunks = chunks.len();

    let mut result = BenchmarkResult::new(bench_name("full", total_reads, num_chunks));

    for i in 0..config.iterations {
        let iter_t = Instant::now();
        let mut total_duration = Duration::ZERO;

        for (chunk_idx, chunk) in chunks.iter().enumerate() {
            // Each chunk submission needs a fresh client with up-to-date state,
            // because the previous submission changes the account nonce on the network.
            let mut client = create_iteration_client(config).await?;
            client.sync_state().await?;

            let tx_request = build_chunk_tx_request(&client, chunk, slot_infos)?;

            // Measure full transaction time (execute + prove + submit)
            let (_, duration) = Box::pin(measure_time_async(|| async {
                Box::pin(client.submit_new_transaction(account_id, tx_request)).await
            }))
            .await;

            total_duration += duration;

            // Wait for the block to advance after every submission so the node
            // has processed the transaction before the next chunk or iteration.
            // Skip only after the very last submission of the entire benchmark.
            let is_last = i == config.iterations - 1 && chunk_idx == num_chunks - 1;
            if !is_last {
                wait_for_block_advancement(&mut client).await?;
            }
        }

        result.add_iteration(total_duration);
        println!(
            "  Iteration {}/{}: {:.2?} (total: {:.2?})",
            i + 1,
            config.iterations,
            total_duration,
            iter_t.elapsed()
        );
    }

    result = result.with_metadata(format!(
        "Full transaction (execute + prove + submit), {total_reads} storage reads{}",
        if num_chunks > 1 {
            format!(" across {num_chunks} transactions")
        } else {
            String::new()
        }
    ));

    Ok(result)
}

// SLOT DETECTION
// ================================================================================================

/// Builds slot infos from the imported account storage.
///
/// Only includes bench map slots (`miden::bench::map_slot_N`), returned in canonical
/// index order (0, 1, 2, ...) to match the account's reader component. This ensures
/// the dynamically-linked reader procedures have matching MAST roots.
fn build_slot_infos_from_storage(
    storage: &miden_client::account::AccountStorage,
) -> Vec<StorageSlotInfo> {
    // Collect bench map slots with their parsed indices
    let mut indexed: Vec<(usize, Vec<StorageMapKey>)> = storage
        .slots()
        .iter()
        .filter_map(|slot| {
            let name = slot.name().to_string();
            let idx = name.strip_prefix("miden::bench::map_slot_")?.parse::<usize>().ok()?;
            if let StorageSlotContent::Map(map) = slot.content() {
                let keys: Vec<StorageMapKey> = map.entries().map(|(k, _v)| *k).collect();
                Some((idx, keys))
            } else {
                None
            }
        })
        .collect();

    if indexed.is_empty() {
        return Vec::new();
    }

    indexed.sort_by_key(|(idx, _)| *idx);
    let max_idx = indexed.last().unwrap().0;

    // Build contiguous slot_infos [0..=max_idx] so procedure indices match the account's
    // reader component. Slots missing from storage get empty key lists (no reads generated).
    let mut keys_by_idx: Vec<Vec<StorageMapKey>> = vec![Vec::new(); max_idx + 1];
    for (idx, keys) in indexed {
        keys_by_idx[idx] = keys;
    }

    keys_by_idx
        .into_iter()
        .enumerate()
        .map(|(i, keys)| StorageSlotInfo {
            name: format!("miden::bench::map_slot_{i}"),
            keys,
        })
        .collect()
}

// READ OPS & CHUNKING
// ================================================================================================

/// Flattens slot infos into individual read operations.
fn flatten_read_ops(slot_infos: &[StorageSlotInfo]) -> Vec<ReadOp> {
    slot_infos
        .iter()
        .enumerate()
        .flat_map(|(idx, info)| info.keys.iter().map(move |k| ReadOp { slot_idx: idx, key: *k }))
        .collect()
}

/// Splits read operations into chunks of at most `max_reads` each.
fn chunk_read_ops(all_ops: &[ReadOp], max_reads: usize) -> Vec<Vec<ReadOp>> {
    if all_ops.len() <= max_reads {
        return vec![all_ops.to_vec()];
    }
    all_ops.chunks(max_reads).map(<[ReadOp]>::to_vec).collect()
}

// HELPERS
// ================================================================================================

/// Creates a fresh client for a benchmark iteration.
///
/// Each iteration uses a new client to avoid state leaking between measurements.
/// The client connects to the same persistent store directory (populated by `deploy`),
/// which contains the `SQLite` database and filesystem keystore.
async fn create_iteration_client(
    config: &BenchConfig,
) -> anyhow::Result<Client<FilesystemKeyStore>> {
    config::create_client(&config.network, &config.store_path).await
}

/// Formats the benchmark name with optional chunk count.
fn bench_name(phase: &str, total_reads: usize, num_chunks: usize) -> String {
    if num_chunks > 1 {
        format!("{phase} ({total_reads} storage reads, {num_chunks} txs)")
    } else {
        format!("{phase} ({total_reads} storage reads)")
    }
}

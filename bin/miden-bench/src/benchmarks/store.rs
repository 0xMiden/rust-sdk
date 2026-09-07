//! Benchmarks the `SQLite` store methods against a growing database.
//!
//! Each size in the sweep seeds its own database file, so the numbers of one size never depend on
//! the leftovers of another. What matters in the output is the growth between the smallest and the
//! largest size: a query served by an index stays flat, one that falls back to a scan does not.
//!
//! Within a size, the read measurements run before the writing ones, so every read sees exactly
//! the seeded database.

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use miden_client::ONE;
use miden_client::account::{Account, AccountId, Address};
use miden_client::block::BlockNumber;
use miden_client::note::{InputNoteReader, NoteUpdateTracker};
use miden_client::store::{ClientAccountType, InputNoteCursor, NoteFilter, Store};
use miden_client::sync::{
    AccountUpdates,
    PartialBlockchainUpdates,
    StateSyncUpdate,
    TransactionUpdateTracker,
};
use miden_client_sqlite_store::SqliteStore;

use crate::generators::store_data;
use crate::metrics::BenchmarkResult;
use crate::report::ScalingPoint;

/// Notes written by each of the two insert measurements. It is a batch a sync could realistically
/// carry, and it stays constant across sizes so the insert cost is comparable between them.
const INSERT_BATCH_SIZE: usize = 50;

/// Results of both sweeps, ready to be printed as one table each.
pub struct StoreBenchmarks {
    /// One point per note count.
    pub notes: Vec<ScalingPoint>,
    /// One point per account count.
    pub accounts: Vec<ScalingPoint>,
}

/// Runs the note-count and account-count sweeps, seeding one database per size under `workdir`.
pub async fn run_store_benchmarks(
    note_counts: &[usize],
    account_counts: &[usize],
    iterations: usize,
    workdir: &Path,
) -> anyhow::Result<StoreBenchmarks> {
    let mut notes = Vec::new();
    for &count in note_counts {
        println!("Seeding and measuring {count} notes...");
        notes.push(ScalingPoint {
            label: format!("{count} notes"),
            results: bench_note_methods(workdir, count, iterations).await?,
        });
    }

    let mut accounts = Vec::new();
    for &count in account_counts {
        println!("Seeding and measuring {count} accounts...");
        accounts.push(ScalingPoint {
            label: format!("{count} accounts"),
            results: bench_account_methods(workdir, count, iterations).await?,
        });
    }

    Ok(StoreBenchmarks { notes, accounts })
}

// NOTE METHODS
// ================================================================================================

async fn bench_note_methods(
    workdir: &Path,
    count: usize,
    iterations: usize,
) -> anyhow::Result<Vec<BenchmarkResult>> {
    let store = SqliteStore::new(workdir.join(format!("notes-{count}.sqlite3")))
        .await
        .context("failed to create the note benchmark store")?;

    let consumer = store_data::consumer_account_id();
    let seed = store_data::note_seed(consumer, count);

    store.upsert_input_notes(&seed.consumed).await?;
    store.upsert_input_notes(&seed.unspent).await?;
    for (header, has_client_notes) in &seed.block_headers {
        store.insert_block_header(header, &[], *has_client_notes).await?;
    }

    let mut results = note_read_measurements(&store, &seed, consumer, iterations).await?;
    let inserts = note_write_measurements(&store, &seed, consumer, iterations).await?;

    // The walk runs last, over the seeded notes plus whatever the inserts added, so its per-note
    // cost is divided by the notes actually returned.
    let store: Arc<dyn Store> = Arc::new(store);
    let mut walked = 0u32;
    let walk = measure("InputNoteReader [full walk]", iterations, async |_| {
        let mut reader = InputNoteReader::new(store.clone(), consumer);
        let mut count = 0;
        while reader.next().await?.is_some() {
            count += 1;
        }
        walked = count;
        Ok(())
    })
    .await?;

    results.push(per_note(&walk, walked));
    results.push(walk);
    results.extend(inserts);

    Ok(results)
}

/// Measures the read paths against the seeded database.
async fn note_read_measurements(
    store: &SqliteStore,
    seed: &store_data::NoteSeed,
    consumer: AccountId,
    iterations: usize,
) -> anyhow::Result<Vec<BenchmarkResult>> {
    // The point lookups below read notes the seed holds, so each one is a lookup that hits rather
    // than one that stops at the index.
    let last_consumed = seed
        .consumed
        .last()
        .context("a note benchmark needs at least one consumed note")?;
    let note_id = last_consumed.id().context("a consumed note carries an id")?;
    let nullifier = last_consumed.nullifier().context("a consumed note carries a nullifier")?;
    let script_root = store_data::note_scripts()[0].root();

    // The cursor of the second-to-last note, so the seek has the whole history in front of it.
    let deep_cursor = {
        let mut ordered: Vec<_> = seed.consumed.iter().collect();
        ordered.sort_by_key(|note| store_data::consumption_key(note));
        let index = ordered.len().saturating_sub(2);
        InputNoteCursor::from_record(ordered[index]).context("a consumed note yields a cursor")?
    };

    Ok(vec![
        measure("get_input_notes(Unspent)", iterations, async |_| {
            store.get_input_notes(NoteFilter::Unspent).await?;
            Ok(())
        })
        .await?,
        measure("get_input_notes(Consumed)", iterations, async |_| {
            store.get_input_notes(NoteFilter::Consumed).await?;
            Ok(())
        })
        .await?,
        measure("get_input_notes(List) [1 note]", iterations, async |_| {
            store.get_input_notes(NoteFilter::List(vec![note_id])).await?;
            Ok(())
        })
        .await?,
        measure("get_input_notes(Nullifiers) [1 note]", iterations, async |_| {
            store.get_input_notes(NoteFilter::Nullifiers(vec![nullifier])).await?;
            Ok(())
        })
        .await?,
        measure("get_input_notes(ScriptRoots) [1 root]", iterations, async |_| {
            store.get_input_notes(NoteFilter::ScriptRoots(vec![script_root])).await?;
            Ok(())
        })
        .await?,
        measure("get_unspent_input_note_nullifiers", iterations, async |_| {
            store.get_unspent_input_note_nullifiers().await?;
            Ok(())
        })
        .await?,
        measure("get_tracked_block_headers", iterations, async |_| {
            store.get_tracked_block_headers().await?;
            Ok(())
        })
        .await?,
        measure("get_input_note_after [deep cursor]", iterations, async |_| {
            store
                .get_input_note_after(NoteFilter::Consumed, consumer, None, None, Some(deep_cursor))
                .await?;
            Ok(())
        })
        .await?,
    ])
}

/// Measures the write paths against the seeded database. Every iteration writes notes of its own,
/// so each one is an insert into an already-full table and never a replace.
async fn note_write_measurements(
    store: &SqliteStore,
    seed: &store_data::NoteSeed,
    consumer: AccountId,
    iterations: usize,
) -> anyhow::Result<Vec<BenchmarkResult>> {
    let sync_block = BlockNumber::from(u32::try_from(seed.block_headers.len()).unwrap_or(u32::MAX));

    Ok(vec![
        measure(
            &format!("upsert_input_notes [{INSERT_BATCH_SIZE} notes]"),
            iterations,
            async |i| {
                let notes = store_data::insert_batch(consumer, INSERT_BATCH_SIZE, i);
                store.upsert_input_notes(&notes).await?;
                Ok(())
            },
        )
        .await?,
        measure(
            &format!("apply_state_sync [{INSERT_BATCH_SIZE} notes]"),
            iterations,
            async |i| {
                // Offset past the batches the measurement above wrote, so this one inserts too.
                let notes =
                    store_data::insert_batch(consumer, INSERT_BATCH_SIZE, i + iterations + 1);
                let update = StateSyncUpdate::from_parts(
                    sync_block,
                    PartialBlockchainUpdates::default(),
                    NoteUpdateTracker::for_transaction_updates(notes, [], []),
                    TransactionUpdateTracker::default(),
                    AccountUpdates::default(),
                );
                store.apply_state_sync(update).await?;
                Ok(())
            },
        )
        .await?,
    ])
}

/// Returns the per-note cost of a walk over `walked` notes.
fn per_note(walk: &BenchmarkResult, walked: u32) -> BenchmarkResult {
    let mut result = BenchmarkResult::new("InputNoteReader [per note]")
        .with_metadata(format!("{walked} notes walked"));
    for iteration in &walk.iterations {
        result.add_iteration(iteration.checked_div(walked).unwrap_or_default());
    }

    result
}

// ACCOUNT METHODS
// ================================================================================================

async fn bench_account_methods(
    workdir: &Path,
    count: usize,
    iterations: usize,
) -> anyhow::Result<Vec<BenchmarkResult>> {
    let store_path = workdir.join(format!("accounts-{count}.sqlite3"));
    let store = SqliteStore::new(store_path.clone())
        .await
        .context("failed to create the account benchmark store")?;

    let accounts = store_data::wallet_accounts(count)?;
    for account in &accounts {
        store
            .insert_account(account, Address::new(account.id()), ClientAccountType::Native)
            .await?;
    }

    let last = accounts.last().context("an account benchmark needs at least one account")?;
    let last_id = last.id();

    let mut results = vec![
        measure("SqliteStore::new [open]", iterations, async |_| {
            SqliteStore::new(store_path.clone()).await?;
            Ok(())
        })
        .await?,
        measure("get_account_headers", iterations, async |_| {
            store.get_account_headers().await?;
            Ok(())
        })
        .await?,
        measure("get_account_header [single]", iterations, async |_| {
            store.get_account_header(last_id).await?;
            Ok(())
        })
        .await?,
    ];

    results.push(prune_account_history(&store, last.clone(), iterations).await?);

    Ok(results)
}

/// Measures pruning one historical account state. The state is created outside the timed section,
/// once per iteration, because the prune is what the measurement is about.
async fn prune_account_history(
    store: &SqliteStore,
    mut account: Account,
    iterations: usize,
) -> anyhow::Result<BenchmarkResult> {
    let mut result = BenchmarkResult::new("prune_account_history [1 state]");

    for iteration in 0..iterations {
        account.increment_nonce(ONE)?;
        store.update_account(&account).await?;

        let start = Instant::now();
        let deleted = store.prune_account_history(account.id(), account.nonce()).await?;
        result.add_iteration(start.elapsed());

        // The state archived just above has to be what the prune deletes. A prune that finds
        // nothing measures nothing, and would report a flat row for the wrong reason.
        anyhow::ensure!(deleted > 0, "iteration {iteration} pruned no historical state");
    }

    Ok(result)
}

// HELPERS
// ================================================================================================

/// Runs `operation` `iterations` times, recording how long each run took. The iteration index is
/// passed in so that measurements which write can keep every run's data distinct.
async fn measure<F>(
    name: &str,
    iterations: usize,
    mut operation: F,
) -> anyhow::Result<BenchmarkResult>
where
    F: AsyncFnMut(usize) -> anyhow::Result<()>,
{
    let mut result = BenchmarkResult::new(name);

    for iteration in 0..iterations {
        let start = Instant::now();
        operation(iteration).await?;
        result.add_iteration(start.elapsed());
    }

    Ok(result)
}

use std::path::{Path, PathBuf};

use anyhow::Context;
use miden_client::account::AccountId;
use miden_client::note::NoteTag;

use crate::config::{self, BenchConfig};
use crate::metrics::{BenchmarkResult, measure_time_async};

/// Benchmarks [`miden_client::Client::sync_state`] from genesis to the chain tip.
///
/// Each iteration runs against a fresh store directory, so every measurement covers a full sync
/// from genesis. A synced store only has the tip to catch up on, which would measure a single
/// empty RPC round trip instead.
///
/// With `from_store`, each iteration's store is a copy of that snapshot directory instead of an
/// empty store. This is what makes runs comparable across binaries: every iteration starts from
/// byte-identical local state, and no setup RPC lands inside the timed region. A snapshot holding
/// accounts whose stored state is behind the chain is what exercises the public account fetch
/// path, since only diverging accounts are fetched.
///
/// Accounts and tags are registered before the timer starts, along with an RPC round trip that
/// establishes the connection, so scenarios differ only in sync work. Each turns on a different
/// part of the sync:
/// - with no tracked accounts, `sync_transactions` is skipped entirely;
/// - with no tracked tags, `sync_notes_with_content` is skipped entirely.
///
/// Tracking an account also registers its derived tag, so `--tag` isolates the note sync only
/// when no accounts are tracked.
pub async fn run_sync_benchmarks(
    config: &BenchConfig,
    account_ids: Vec<String>,
    tags: Vec<String>,
    account_files: Vec<PathBuf>,
    from_store: Option<&Path>,
    note_transport: Option<&str>,
) -> anyhow::Result<Vec<BenchmarkResult>> {
    let account_ids = account_ids
        .iter()
        .map(|id| AccountId::from_hex(id))
        .collect::<Result<Vec<_>, _>>()?;
    let tags = tags.iter().map(|tag| parse_note_tag(tag)).collect::<anyhow::Result<Vec<_>>>()?;

    for id in &account_ids {
        println!("Tracking account: {id}");
    }
    for tag in &tags {
        println!("Tracking tag: {tag}");
    }
    for file in &account_files {
        println!("Tracking account file: {}", file.display());
    }

    let mut result = BenchmarkResult::new(match from_store {
        Some(snapshot) => format!(
            "sync_state (snapshot {}, +{} accounts, +{} tags)",
            snapshot.display(),
            account_ids.len(),
            tags.len()
        ),
        None => format!(
            "sync_state ({} accounts, {} files, {} tags, transport {})",
            account_ids.len(),
            account_files.len(),
            tags.len(),
            if note_transport.is_some() { "on" } else { "off" },
        ),
    });

    if let Some(snapshot) = from_store {
        println!("Seeding each iteration from snapshot: {}", snapshot.display());
    }

    for i in 0..config.iterations {
        let store_path = config.store_path.join(format!("sync-{i}"));
        match from_store {
            Some(snapshot) => copy_dir(snapshot, &store_path)
                .with_context(|| format!("failed to copy snapshot {}", snapshot.display()))?,
            None => std::fs::create_dir_all(&store_path)?,
        }

        let mut client =
            config::create_client_with_transport(&config.network, &store_path, note_transport)
                .await?;
        for &id in &account_ids {
            client.import_account_by_id(id).await?;
        }
        for &tag in &tags {
            client.add_note_tag(tag).await?;
        }
        // Private accounts cannot be fetched from the node, so their state only reaches the
        // store from a file.
        for file in &account_files {
            crate::import::import_from_file(&mut client, &store_path, file).await?;
        }

        // Establish the RPC connection before the timer starts. The first request on a fresh
        // client pays the TLS and HTTP/2 handshake, which would otherwise be attributed to the
        // sync and swamp the differences between scenarios.
        client.network_id().await?;

        // Registering accounts or tags can advance the store's sync height, which would shrink
        // the range the timed sync has to cover. Report where each iteration actually starts.
        let block_from = client.get_sync_height().await?;

        // A seeded store brings its own tracked accounts and tags, so report what the sync is
        // actually working with rather than only what the flags added.
        if i == 0 {
            println!(
                "Store tracks {} accounts and {} tags",
                client.get_account_headers().await?.len(),
                client.get_note_tags().await?.len(),
            );
        }

        let (summary, duration) = measure_time_async(|| client.sync_state()).await;
        let summary = summary?;
        result.add_iteration(duration);

        println!(
            "  Iteration {}/{}: {duration:.2?} (blocks {}->{}, {} new public, {} committed, \
             {} consumed notes, {} updated accounts)",
            i + 1,
            config.iterations,
            block_from,
            summary.block_num,
            summary.new_public_notes.len(),
            summary.committed_notes.len(),
            summary.consumed_notes.len(),
            summary.updated_accounts.len(),
        );

        // Drop the client so `SQLite` releases the database file before the directory is removed.
        drop(client);
        std::fs::remove_dir_all(&store_path)?;
    }

    Ok(vec![result])
}

/// Recursively copies `from` into `to`, creating `to` if needed.
fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Parses a note tag given as a decimal or `0x`-prefixed hexadecimal `u32`.
fn parse_note_tag(s: &str) -> anyhow::Result<NoteTag> {
    let value = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(hex) => u32::from_str_radix(hex, 16),
        None => s.parse(),
    }
    .with_context(|| format!("invalid note tag `{s}`, expected a decimal or 0x-hex u32"))?;
    Ok(NoteTag::new(value))
}

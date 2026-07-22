#![allow(clippy::similar_names)]
//! Untracked benchmark for the few-heavy-accounts shape: a handful of accounts, each with a
//! large storage map. Complements the many-light-accounts harnesses.
//!
//! Two modes running in separate processes so populate allocations never pollute the run-mode
//! RSS samples:
//!   populate <path> <accounts> <entries>  - create a store, print populate time and file size.
//!   run <path> <accounts> <entries>       - open the store 3 times (timed, RSS sampled around
//!                                           the first), update one map entry on one account,
//!                                           read one witness twice, sample RSS after.
//!   memopen <path> <accounts> <entries>   - open the store once and sample RSS before/after,
//!                                           allocating nothing else, so the delta is clean.
//!
//! Every account's map has distinct values so trees cannot be deduplicated by root.

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

use miden_client::account::component::{AccountComponent, BasicWallet};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountPatch,
    AccountStoragePatch,
    AccountType,
    AccountVaultPatch,
    Address,
    StorageMap,
    StorageMapKey,
    StorageSlot,
    StorageSlotName,
};
use miden_client::auth::{AuthSchemeId, AuthSingleSig, PublicKeyCommitment};
use miden_client::store::{ClientAccountType, Store};
use miden_client::{EMPTY_WORD, Felt, ZERO};
use miden_client_sqlite_store::SqliteStore;
use miden_protocol::account::{
    AccountComponentMetadata, StorageMapPatch, StorageMapPatchEntries, StorageSlotPatch,
};
use miden_standards::account::auth::Approver;

fn slot_name() -> StorageSlotName {
    StorageSlotName::new("miden::bench::forest::map").expect("valid slot name")
}

fn build_account(index: u64, entries: u64) -> anyhow::Result<Account> {
    let mut map = StorageMap::new();
    for i in 1..=entries {
        map.insert(
            StorageMapKey::new([Felt::new_unchecked(i), ZERO, ZERO, ZERO].into()),
            // Distinct value per account so every account's map tree has a unique root.
            [Felt::new_unchecked(i * 100), Felt::new_unchecked(index + 1), ZERO, ZERO].into(),
        )?;
    }

    let component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![StorageSlot::with_map(slot_name(), map)],
        AccountComponentMetadata::new("miden::bench::forest"),
    )?;

    let mut seed = [0u8; 32];
    seed[0..8].copy_from_slice(&index.to_le_bytes());

    let account = AccountBuilder::new(seed)
        .account_type(AccountType::Private)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            PublicKeyCommitment::from(EMPTY_WORD),
            AuthSchemeId::Falcon512Poseidon2,
        )))
        .with_component(component)
        .build_existing()?;
    Ok(account)
}

fn rss_kb() -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps runs");
    String::from_utf8_lossy(&out.stdout).trim().parse().expect("rss is a number")
}

async fn populate(path: PathBuf, accounts: u64, entries: u64) -> anyhow::Result<()> {
    let store = SqliteStore::new(path.clone()).await?;
    let start = Instant::now();
    for i in 0..accounts {
        let account = build_account(i, entries)?;
        store
            .insert_account(&account, Address::new(account.id()), ClientAccountType::Native)
            .await?;
    }
    let elapsed = start.elapsed();
    drop(store);
    println!("PHASE=A ACCTS={accounts} E={entries} MS={:.3}", elapsed.as_secs_f64() * 1000.0);
    let bytes = std::fs::metadata(&path)?.len();
    println!("PHASE=file_size ACCTS={accounts} E={entries} BYTES={bytes}");
    Ok(())
}

async fn run(path: PathBuf, accounts: u64, entries: u64) -> anyhow::Result<()> {
    // Pre-build everything the measured operations need so no unrelated allocation lands
    // between the RSS samples.
    let mut account = build_account(0, entries)?;
    let target_nonce = account.nonce().as_canonical_u64() + 1;
    let mut patch_entries = StorageMapPatchEntries::new();
    patch_entries.insert(
        StorageMapKey::new([Felt::new_unchecked(1), ZERO, ZERO, ZERO].into()),
        [Felt::new_unchecked(999_999), ZERO, ZERO, ZERO].into(),
    );
    let storage_patch = AccountStoragePatch::from_entries([(
        slot_name(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries: patch_entries }),
    )])?;
    let patch = AccountPatch::new(
        account.id(),
        storage_patch,
        AccountVaultPatch::default(),
        None,
        Some(Felt::new_unchecked(target_nonce)),
    )?;
    account.apply_patch(&patch)?;
    let read_key = StorageMapKey::new([Felt::new_unchecked(2), ZERO, ZERO, ZERO].into());
    let slot = slot_name();

    // Open x3: RSS sampled around the first open, the later opens timed in the same process.
    let rss_before = rss_kb();
    let start = Instant::now();
    let store = SqliteStore::new(path.clone()).await?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    let rss_open = rss_kb();
    println!("PHASE=open1 ACCTS={accounts} E={entries} MS={ms:.3}");
    drop(store);
    let mut store = None;
    for open in 2..=3u32 {
        let start = Instant::now();
        let reopened = SqliteStore::new(path.clone()).await?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        println!("PHASE=open{open} ACCTS={accounts} E={entries} MS={ms:.3}");
        store = Some(reopened);
    }
    let store = store.expect("store from last open");

    // One committed single-entry update on the heavy account.
    let start = Instant::now();
    store.update_account(&account).await?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    println!("PHASE=C ACCTS={accounts} E={entries} MS={ms:.3}");

    // Witness reads, first and second.
    for read in 1..=2u32 {
        let start = Instant::now();
        let _ = store.get_account_map_item(account.id(), slot.clone(), read_key).await?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        println!("PHASE=D_read{read} ACCTS={accounts} E={entries} MS={ms:.3}");
    }

    let rss_ops = rss_kb();
    println!(
        "PHASE=mem ACCTS={accounts} E={entries} BEFORE_KB={rss_before} OPEN_KB={rss_open} \
         OPS_KB={rss_ops} OPEN_DELTA_KB={} OPS_DELTA_KB={}",
        rss_open.saturating_sub(rss_before),
        rss_ops.saturating_sub(rss_open),
    );
    Ok(())
}

async fn memopen(path: PathBuf, accounts: u64, entries: u64) -> anyhow::Result<()> {
    let rss_before = rss_kb();
    let store = SqliteStore::new(path).await?;
    let rss_open = rss_kb();
    drop(store);
    println!(
        "PHASE=memopen ACCTS={accounts} E={entries} BEFORE_KB={rss_before} OPEN_KB={rss_open} \
         OPEN_DELTA_KB={}",
        rss_open.saturating_sub(rss_before),
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).expect("mode: populate|run").as_str();
    let path = PathBuf::from(args.get(2).expect("store path"));
    let accounts: u64 = args.get(3).expect("accounts").parse()?;
    let entries: u64 = args.get(4).expect("entries per account").parse()?;
    match mode {
        "populate" => populate(path, accounts, entries).await,
        "run" => run(path, accounts, entries).await,
        "memopen" => memopen(path, accounts, entries).await,
        other => anyhow::bail!("unknown mode {other}"),
    }
}

//! Untracked benchmark measuring the memory footprint of the account SMT forest.
//!
//! Two modes running in separate processes so populate allocations never pollute the
//! open-mode numbers:
//!   populate <path> <n>  - create a store with N accounts (distinct 50-entry map each),
//!                          print the resulting SQLite file size.
//!   open <path> <n>      - measure process RSS before opening the store, after opening it,
//!                          and after 20 witness reads.
//!
//! RSS is sampled with `ps -o rss=` (KiB). Distinct per-account maps prevent the in-memory
//! forest from deduplicating trees by root, so the old implementation's footprint reflects
//! one tree per account.
//!
//! Run with: cargo run --release --example mem_bench -p miden-client-sqlite-store -- <mode> <path> <n>

use std::path::PathBuf;
use std::process::Command;

use miden_client::account::component::{AccountComponent, BasicWallet};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountType,
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
use miden_protocol::account::AccountComponentMetadata;
use miden_standards::account::auth::Approver;

const MAP_ENTRIES: u64 = 50;
const READS: u64 = 20;

fn slot_name() -> StorageSlotName {
    StorageSlotName::new("miden::bench::forest::map").expect("valid slot name")
}

fn build_account(index: u64) -> anyhow::Result<Account> {
    let mut map = StorageMap::new();
    for i in 1..=MAP_ENTRIES {
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

async fn populate(path: PathBuf, n: u64) -> anyhow::Result<()> {
    let store = SqliteStore::new(path.clone()).await?;
    for i in 0..n {
        let account = build_account(i)?;
        store
            .insert_account(&account, Address::new(account.id()), ClientAccountType::Native)
            .await?;
    }
    drop(store);
    let bytes = std::fs::metadata(&path)?.len();
    println!("PHASE=file_size N={n} BYTES={bytes}");
    Ok(())
}

async fn open_mode(path: PathBuf, n: u64) -> anyhow::Result<()> {
    // Pre-build everything the read loop needs so no unrelated allocation lands between
    // the RSS samples.
    let reads = READS.min(n);
    let ids: Vec<_> = (0..reads).map(|i| build_account(i).map(|a| a.id())).collect::<Result<_, _>>()?;
    let key = StorageMapKey::new([Felt::new_unchecked(2), ZERO, ZERO, ZERO].into());
    let slot = slot_name();

    let rss_before = rss_kb();
    let store = SqliteStore::new(path).await?;
    let rss_open = rss_kb();
    for id in &ids {
        let _ = store.get_account_map_item(*id, slot.clone(), key).await?;
    }
    let rss_reads = rss_kb();

    println!(
        "PHASE=mem N={n} BEFORE_KB={rss_before} OPEN_KB={rss_open} READS_KB={rss_reads} \
         OPEN_DELTA_KB={} READS_DELTA_KB={}",
        rss_open - rss_before,
        rss_reads - rss_open,
    );
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).expect("mode: populate|open").as_str();
    let path = PathBuf::from(args.get(2).expect("store path"));
    let n: u64 = args.get(3).expect("N").parse()?;
    match mode {
        "populate" => populate(path, n).await,
        "open" => open_mode(path, n).await,
        other => anyhow::bail!("unknown mode {other}"),
    }
}

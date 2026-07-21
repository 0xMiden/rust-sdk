//! Untracked benchmark isolating the rollback-protection cost of store writes.
//!
//! Every account gets a DISTINCT 50-entry storage map (values include the account index), so
//! the in-memory forest cannot deduplicate trees by root. On the old implementation each write
//! deep-clones the whole forest as a rollback backup, so per-write cost grows with N; the new
//! implementation relies on the SQL transaction for rollback and pays no clone.
//!
//! Run with: cargo run --release --example rollback_bench -p miden-client-sqlite-store

use std::path::PathBuf;
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
use miden_client::testing::common::create_test_store_path;
use miden_client::{EMPTY_WORD, Felt, ZERO};
use miden_client_sqlite_store::SqliteStore;
use miden_protocol::account::{
    AccountComponentMetadata, StorageMapPatch, StorageMapPatchEntries, StorageSlotPatch,
};
use miden_standards::account::auth::Approver;

const MAP_ENTRIES: u64 = 50;
const WRITES: usize = 10;

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

async fn run(n: u64) -> anyhow::Result<()> {
    let store_path: PathBuf = create_test_store_path();

    // PHASE A: populate a fresh store with N accounts (distinct map per account).
    let store = SqliteStore::new(store_path.clone()).await?;
    let mut write_accounts: Vec<Account> = Vec::new();
    let start = Instant::now();
    for i in 0..n {
        let account = build_account(i)?;
        if write_accounts.len() < WRITES {
            write_accounts.push(account.clone());
        }
        store
            .insert_account(&account, Address::new(account.id()), ClientAccountType::Native)
            .await?;
    }
    let elapsed = start.elapsed();
    println!("PHASE=A N={n} MS={:.3}", elapsed.as_secs_f64() * 1000.0);

    // PHASE W: single-entry updates on distinct accounts, timed individually. On the old
    // implementation each one pays the full-forest clone that exists only as rollback backup.
    let mut total_ms = 0.0;
    for (w, acc) in write_accounts.iter_mut().enumerate() {
        let target_nonce = acc.nonce().as_canonical_u64() + 1;
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(
            StorageMapKey::new([Felt::new_unchecked(1), ZERO, ZERO, ZERO].into()),
            [Felt::new_unchecked(999_999), Felt::new_unchecked(w as u64 + 1), ZERO, ZERO].into(),
        );
        let storage_patch = AccountStoragePatch::from_entries([(
            slot_name(),
            StorageSlotPatch::Map(StorageMapPatch::Update { entries }),
        )])?;
        let patch = AccountPatch::new(
            acc.id(),
            storage_patch,
            AccountVaultPatch::default(),
            None,
            Some(Felt::new_unchecked(target_nonce)),
        )?;
        acc.apply_patch(&patch)?;

        let start = Instant::now();
        store.update_account(acc).await?;
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        total_ms += ms;
        println!("PHASE=W{w} N={n} MS={ms:.3}");
    }
    println!("PHASE=W_avg N={n} MS={:.3}", total_ms / WRITES as f64);

    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let ns: Vec<u64> = if args.len() > 1 {
        args[1..].iter().map(|a| a.parse().expect("N must be a number")).collect()
    } else {
        vec![100, 1000]
    };
    for n in ns {
        run(n).await?;
    }
    Ok(())
}

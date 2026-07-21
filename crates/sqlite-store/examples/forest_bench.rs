//! Untracked benchmark comparing SMT-forest store performance (open / write / read).
//! Run with: cargo run --release --example forest_bench -p miden-client-sqlite-store

use std::path::PathBuf;
use std::time::Instant;

use miden_client::account::component::{AccountComponent, BasicWallet};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountId,
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
    AccountComponentMetadata,
    StorageMapPatch,
    StorageMapPatchEntries,
    StorageSlotPatch,
};
use miden_standards::account::auth::Approver;

const MAP_ENTRIES: u64 = 50;
const SESSION_WRITES: usize = 20;
const SESSION_READS: usize = 200;

fn slot_name() -> StorageSlotName {
    StorageSlotName::new("miden::bench::forest::map").expect("valid slot name")
}

fn build_account(index: u64) -> anyhow::Result<Account> {
    let mut map = StorageMap::new();
    for i in 1..=MAP_ENTRIES {
        map.insert(
            StorageMapKey::new([Felt::new_unchecked(i), ZERO, ZERO, ZERO].into()),
            [Felt::new_unchecked(i * 100), ZERO, ZERO, ZERO].into(),
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

    // PHASE A: populate a fresh store with N accounts.
    let store = SqliteStore::new(store_path.clone()).await?;
    let mut first_account: Option<Account> = None;
    let mut session_accounts: Vec<Account> = Vec::new();
    let mut all_ids: Vec<AccountId> = Vec::new();
    let start = Instant::now();
    for i in 0..n {
        let account = build_account(i)?;
        if first_account.is_none() {
            first_account = Some(account.clone());
        }
        if i >= 1 && session_accounts.len() < SESSION_WRITES {
            session_accounts.push(account.clone());
        }
        all_ids.push(account.id());
        store
            .insert_account(&account, Address::new(account.id()), ClientAccountType::Native)
            .await?;
    }
    let elapsed = start.elapsed();
    println!("PHASE=A N={n} MS={:.3}", elapsed.as_secs_f64() * 1000.0);

    // PHASE B: drop and reopen the store from the same file, 3 times.
    drop(store);
    for open in 1..=3u32 {
        let start = Instant::now();
        let reopened = SqliteStore::new(store_path.clone()).await?;
        let elapsed = start.elapsed();
        println!("PHASE=B_open{open} N={n} MS={:.3}", elapsed.as_secs_f64() * 1000.0);
        drop(reopened);
    }

    // Reopen once more, kept for the write/read phases.
    let store = SqliteStore::new(store_path.clone()).await?;

    // PHASE C: modify one map entry on one account and persist via update_account.
    let mut account = first_account.expect("at least one account");
    let account_id = account.id();
    let target_nonce = account.nonce().as_canonical_u64() + 1;
    let mut entries = StorageMapPatchEntries::new();
    entries.insert(
        StorageMapKey::new([Felt::new_unchecked(1), ZERO, ZERO, ZERO].into()),
        [Felt::new_unchecked(999_999), ZERO, ZERO, ZERO].into(),
    );
    let storage_patch = AccountStoragePatch::from_entries([(
        slot_name(),
        StorageSlotPatch::Map(StorageMapPatch::Update { entries }),
    )])?;
    let patch = AccountPatch::new(
        account_id,
        storage_patch,
        AccountVaultPatch::default(),
        None,
        Some(Felt::new_unchecked(target_nonce)),
    )?;
    account.apply_patch(&patch)?;

    let start = Instant::now();
    store.update_account(&account).await?;
    let elapsed = start.elapsed();
    println!("PHASE=C N={n} MS={:.3}", elapsed.as_secs_f64() * 1000.0);

    // PHASE D: witness reads (first and second, to expose any caching difference).
    let map_key = StorageMapKey::new([Felt::new_unchecked(2), ZERO, ZERO, ZERO].into());

    let start = Instant::now();
    let _ = store.get_account_map_item(account_id, slot_name(), map_key).await?;
    let elapsed = start.elapsed();
    println!("PHASE=D_read1 N={n} MS={:.3}", elapsed.as_secs_f64() * 1000.0);

    let start = Instant::now();
    let _ = store.get_account_map_item(account_id, slot_name(), map_key).await?;
    let elapsed = start.elapsed();
    println!("PHASE=D_read2 N={n} MS={:.3}", elapsed.as_secs_f64() * 1000.0);

    // PHASE E: end-to-end session. Open the store, apply SESSION_WRITES single-entry updates on
    // distinct accounts, then SESSION_READS witness reads spread across all accounts, measured
    // as one wall-clock total (what a client session actually pays).
    drop(store);
    let start = Instant::now();
    let store = SqliteStore::new(store_path.clone()).await?;
    for acc in session_accounts.iter_mut() {
        let target_nonce = acc.nonce().as_canonical_u64() + 1;
        let mut entries = StorageMapPatchEntries::new();
        entries.insert(
            StorageMapKey::new([Felt::new_unchecked(3), ZERO, ZERO, ZERO].into()),
            [Felt::new_unchecked(777_777), ZERO, ZERO, ZERO].into(),
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
        store.update_account(acc).await?;
    }
    for r in 0..SESSION_READS {
        let id = all_ids[r % all_ids.len()];
        let key_idx = (r % MAP_ENTRIES as usize) as u64 + 1;
        let key = StorageMapKey::new([Felt::new_unchecked(key_idx), ZERO, ZERO, ZERO].into());
        let _ = store.get_account_map_item(id, slot_name(), key).await?;
    }
    let elapsed = start.elapsed();
    println!("PHASE=E_session N={n} MS={:.3}", elapsed.as_secs_f64() * 1000.0);

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

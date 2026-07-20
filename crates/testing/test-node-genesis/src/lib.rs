//! Generates the genesis fixtures used to bootstrap a testing node from the standalone Miden node
//! executables. The accounts only depend on `miden-protocol`/`miden-standards`, so the generated
//! configuration is independent of the node's own crates.

pub mod agglayer;

use std::fmt::Write as _;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use ::rand::{RngExt, random};
use anyhow::{Context, Result};
use miden_protocol::account::auth::{AuthScheme, AuthSecretKey};
use miden_protocol::account::{
    Account,
    AccountBuilder,
    AccountComponent,
    AccountComponentMetadata,
    AccountFile,
    AccountType,
    StorageMap,
    StorageMapKey,
};
use miden_protocol::asset::{Asset, AssetAmount, FungibleAsset, TokenSymbol};
use miden_protocol::{ONE, Word};
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::faucets::{
    FungibleFaucet,
    TokenName,
    create_singlesig_user_fungible_faucet,
};
use miden_standards::account::policies::{BurnPolicy, MintPolicy, TokenPolicyManager};
use miden_standards::account::wallets::BasicWallet;
use miden_standards::testing::faucet::user_faucet_single_sig_acl;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

// GENESIS CONFIG GENERATION
// ================================================================================================

/// Genesis faucet file name. Carries the secret key so the operator/tests can mint TST.
pub const GENESIS_FAUCET_FILE: &str = "tst_faucet.mac";

/// Writes the genesis fixtures into `output_dir` so the node can be bootstrapped with
/// `miden-validator bootstrap --genesis-config-file <output_dir>/genesis.toml`.
///
/// This emits the TST genesis faucet (written with its secret key), the test faucets, and the
/// `too_many_assets` account as `.mac` files referenced by `[[account]]` entries in
/// `genesis.toml`, which the node loads verbatim.
///
/// The native faucet is left unset, so the node mints the default `MIDEN` faucet for fees. With
/// `verification_base_fee = 0` fees are never charged, so the native faucet's identity does not
/// affect tests.
///
/// When `include_agglayer` is set, the agglayer genesis accounts (bridge admin, GER manager,
/// bridge, and faucet) are also emitted and included in genesis; integration tests load their
/// `.mac` files via the `AGGLAYER_ACCOUNTS_DIR` env var.
pub fn write_genesis_config(output_dir: &Path, include_agglayer: bool) -> Result<()> {
    std::fs::create_dir_all(output_dir).with_context(|| {
        format!("failed to create genesis output directory {}", output_dir.display())
    })?;

    let mut account_files = Vec::new();

    // Genesis faucet (TST), written with its secret key so it can sign minting transactions.
    let genesis_faucet =
        generate_genesis_account().context("failed to create genesis faucet account")?;
    genesis_faucet
        .write(output_dir.join(GENESIS_FAUCET_FILE))
        .with_context(|| format!("failed to write {GENESIS_FAUCET_FILE}"))?;
    account_files.push(GENESIS_FAUCET_FILE.to_string());

    // Test faucets and the `too_many_assets` account. These are read-only fixtures, so their
    // `.mac` files omit secret keys (only the account is needed in genesis).
    let test_accounts =
        build_test_faucets_and_account().context("failed to build test faucets and account")?;
    for (index, account) in test_accounts.into_iter().enumerate() {
        let file_name = format!("test_account_{index:04}.mac");
        AccountFile::new(account, vec![])
            .write(output_dir.join(&file_name))
            .with_context(|| format!("failed to write {file_name}"))?;
        account_files.push(file_name);
    }

    // Agglayer accounts are written with their secret keys (where applicable) so tests can sign
    // transactions on behalf of the bridge admin and GER manager.
    if include_agglayer {
        let agglayer_accounts = agglayer::create_agglayer_genesis_accounts()
            .context("failed to create agglayer genesis accounts")?;
        for (file_name, account_file) in agglayer_accounts {
            account_file
                .write(output_dir.join(file_name))
                .with_context(|| format!("failed to write {file_name}"))?;
            account_files.push(file_name.to_string());
        }
    }

    let timestamp: u32 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current timestamp should be greater than unix epoch")
        .as_secs()
        .try_into()
        .expect("timestamp should fit into u32");

    let mut toml = format!(
        "version = 1\ntimestamp = {timestamp}\n\n[fee_parameters]\nverification_base_fee = 0\n"
    );
    for file_name in &account_files {
        write!(toml, "\n[[account]]\npath = \"{file_name}\"\n")
            .expect("writing to a String cannot fail");
    }

    std::fs::write(output_dir.join("genesis.toml"), toml)
        .with_context(|| "failed to write genesis.toml")?;

    Ok(())
}

// GENESIS ACCOUNTS
// ================================================================================================

fn generate_genesis_account() -> anyhow::Result<AccountFile> {
    let mut rng = ChaCha20Rng::from_seed(random());
    let secret = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut rng);

    let auth_component = user_faucet_single_sig_acl(
        secret.public_key().to_commitment(),
        AuthScheme::Falcon512Poseidon2,
    );

    let symbol = TokenSymbol::try_from("TST").expect("TST should be a valid token symbol");
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(12)
        .max_supply(AssetAmount::new(1_000_000_000_000).unwrap())
        .build()?;
    let account = create_singlesig_user_fungible_faucet(
        rng.random(),
        faucet,
        auth_component,
        allow_all_policy_manager(),
        AccountType::Public,
    )?;

    // Force the account nonce to 1.
    //
    // By convention, a nonce of zero indicates a freshly generated local account that has yet
    // to be deployed. An account is deployed onchain along within its first transaction which
    // results in a non-zero nonce onchain.
    //
    // The genesis block is special in that accounts are "deployed" without transactions and
    // therefore we need bump the nonce manually to uphold this invariant.
    let (id, vault, storage, code, ..) = account.into_parts();
    let updated_account = Account::new_unchecked(id, vault, storage, code, ONE, None);

    Ok(AccountFile::new(updated_account, vec![secret]))
}

/// Expected account ID produced by [`TEST_ACCOUNT_SEED`] under the current `FungibleFaucet`
/// component layout, policy components, and schema commitments. Used to verify deterministic
/// account generation; update this constant if any input to ID derivation changes.
const TEST_ACCOUNT_ID: &str = "0x0a0a0a0a0a0a0a110a0a0a0a0a0a0a";

/// Deterministic seed used for the test account to ensure reproducible account IDs.
const TEST_ACCOUNT_SEED: [u8; 32] = [0xa; 32];

/// Number of faucets to create. This should exceed the `AccountVaultDetails::MAX_RETURN_ENTRIES`
/// limit defined in the node, so the account triggers `too_many_assets` flag during testing.
const NUM_TEST_FAUCETS: u128 = 1001;

/// Number storage map entries to create. This should exceed the
/// `AccountStorageMapDetails::MAX_RETURN_ENTRIES` limit defined in the node, so the slot
/// triggers `too_many_entries` flag during testing.
const NUM_STORAGE_MAP_ENTRIES: u32 = 1001;

const FAUCET_DECIMALS: u8 = 12;
const FAUCET_MAX_SUPPLY: u32 = 1 << 30;
const ASSET_AMOUNT_PER_FAUCET: u64 = 75;

/// Builds test faucets and an account that triggers the `too_many_assets` flag
/// when requested from the node. This is used to test edge cases in account
/// retrieval and asset handling.
fn build_test_faucets_and_account() -> anyhow::Result<Vec<Account>> {
    let mut rng = ChaCha20Rng::from_seed(random());
    let secret = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut rng);

    let faucets = create_test_faucets(&secret)?;
    let account = create_test_account_with_many_assets(&faucets)?;

    assert_eq!(
        account.id().to_hex(),
        TEST_ACCOUNT_ID,
        "test account was generated with a different id than expected; \
         this may indicate a change in account generation logic"
    );

    Ok([&faucets[..], &[account][..]].concat())
}

/// Creates multiple fungible faucets for testing purposes.
/// Each faucet's index-derived seed gives it a distinct ID within a genesis, but IDs are not
/// stable across runs: the shared auth key is randomly seeded and feeds ID derivation.
fn create_test_faucets(secret: &AuthSecretKey) -> anyhow::Result<Vec<Account>> {
    (0..NUM_TEST_FAUCETS)
        .map(|i| create_single_test_faucet(i, secret))
        .collect::<Result<Vec<_>>>()
        .map_err(|err| anyhow::Error::msg(format!("failed to create test faucets: {err}")))
}

fn create_single_test_faucet(index: u128, secret: &AuthSecretKey) -> anyhow::Result<Account> {
    let init_seed: [u8; 32] = [index.to_be_bytes(), index.to_be_bytes()]
        .concat()
        .try_into()
        .expect("concatenating two 16-byte arrays yields exactly 32 bytes");

    let auth_component = user_faucet_single_sig_acl(
        secret.public_key().to_commitment(),
        AuthScheme::Falcon512Poseidon2,
    );

    let symbol = TokenSymbol::new("TKN")?;
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let faucet_component = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(FAUCET_DECIMALS)
        .max_supply(AssetAmount::new(u64::from(FAUCET_MAX_SUPPLY)).unwrap())
        .build()?;
    let faucet = create_singlesig_user_fungible_faucet(
        init_seed,
        faucet_component,
        auth_component,
        allow_all_policy_manager(),
        AccountType::Public,
    )?;

    // Set nonce to ONE to indicate the account is deployed (see generate_genesis_account)
    let (id, vault, storage, code, ..) = faucet.into_parts();
    Ok(Account::new_unchecked(id, vault, storage, code, ONE, None))
}

/// Creates a test account holding assets from all provided faucets.
/// The account also includes a large storage map to test storage capacity limits.
fn create_test_account_with_many_assets(faucets: &[Account]) -> anyhow::Result<Account> {
    let sk = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut ChaCha20Rng::from_seed(
        TEST_ACCOUNT_SEED,
    ));

    let storage_map = create_large_storage_map();
    let acc_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![storage_map],
        AccountComponentMetadata::new("miden::testing::basic_wallet"),
    )
    .expect("basic wallet component should satisfy account component requirements");

    let assets = faucets.iter().map(|faucet| {
        Asset::Fungible(
            FungibleAsset::new(faucet.id(), ASSET_AMOUNT_PER_FAUCET)
                .expect("faucet id should be valid for asset creation"),
        )
    });

    let account = AccountBuilder::new(TEST_ACCOUNT_SEED)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            sk.public_key().to_commitment(),
            AuthScheme::Falcon512Poseidon2,
        )))
        .account_type(AccountType::Public)
        .with_component(acc_component)
        .with_assets(assets)
        .build_existing()?;

    Ok(account)
}

fn allow_all_policy_manager() -> TokenPolicyManager {
    // Only mint/burn — registering transfer policies installs asset-callback slots on the
    // faucet, which forces minted assets to carry `AssetCallbackFlag::Enabled`. Tests build
    // assets via `FungibleAsset::new`, which defaults to `Disabled`, so adding transfer
    // policies makes `mint_and_send` reject the mint with
    // `ERR_FUNGIBLE_MINT_NOTE_ASSET_NOT_FROM_THIS_FAUCET`.
    TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::allow_all())
        .active_burn_policy(BurnPolicy::allow_all())
        .build()
}

/// Creates a storage map with many entries for stress-testing storage handling.
fn create_large_storage_map() -> miden_protocol::account::StorageSlot {
    let map_entries = (0..NUM_STORAGE_MAP_ENTRIES)
        .map(|i| (StorageMapKey::new(Word::from([i; 4])), Word::from([i; 4])));

    miden_protocol::account::StorageSlot::with_map(
        miden_protocol::account::StorageSlotName::new("miden::test_account::map::too_many_entries")
            .expect("slot name should be valid"),
        StorageMap::with_entries(map_entries).expect("map entries should be valid"),
    )
}

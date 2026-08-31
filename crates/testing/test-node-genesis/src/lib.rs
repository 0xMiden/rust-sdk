//! Generates the genesis fixtures used to bootstrap a testing node from the standalone Miden node
//! executables. The accounts only depend on `miden-protocol`/`miden-standards`, so the generated
//! configuration is independent of the node's own crates.

pub mod agglayer;

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
    AccountId,
    AccountType,
    StorageMap,
    StorageMapKey,
};
use miden_protocol::asset::{Asset, AssetAmount, FungibleAsset, TokenSymbol};
use miden_protocol::{ONE, Word};
use miden_standards::account::access::AccessControl;
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::faucets::{
    FungibleFaucet,
    TokenName,
    create_network_fungible_faucet,
    create_singlesig_user_fungible_faucet,
};
use miden_standards::account::fees::{BasicConstantFeePolicy, FeePolicyManager};
use miden_standards::account::policies::{
    BurnPolicy,
    MintPolicy,
    TokenPolicyManager,
    TransferPolicy,
};
use miden_standards::account::wallets::{BasicWallet, create_basic_wallet};
use miden_standards::note::{BurnNote, MintNote};
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;
use serde::Serialize;

// GENESIS CONFIG GENERATION
// ================================================================================================

/// Genesis faucet file name. Carries the secret key so the operator/tests can mint TST.
pub const GENESIS_FAUCET_FILE: &str = "tst_faucet.mac";

/// Number of funder wallets a fee-charging genesis declares when no count is given.
///
/// A wallet is claimed only for the length of one payment, so this covers the payments in flight
/// at once, which the test runner's thread cap bounds to a handful.
pub const DEFAULT_NUM_FUNDER_WALLETS: u32 = 16;

/// Balance, in base units of the native fee asset, each funder wallet holds at genesis. Covers the
/// funder's own fees plus a handout to every account a single test creates.
const FUNDER_WALLET_BALANCE: u64 = 1_000_000_000;

/// Native fee faucet file name. Carries no secret key: the faucet is signed for by its operator,
/// whose key is in [`FAUCET_OPERATOR_FILE`].
pub const NATIVE_FAUCET_FILE: &str = "native_faucet.mac";

/// File name of the wallet owning the native fee faucet, written with its secret key. It is the
/// account `miden-faucet init --import` takes to dispense the native asset on this chain.
pub const FAUCET_OPERATOR_FILE: &str = "faucet_operator.mac";

/// Token symbol, decimals and max supply of the native fee faucet, matching what the node would
/// generate for it if genesis left it unset.
const NATIVE_FAUCET_SYMBOL: &str = "MIDEN";
const NATIVE_FAUCET_DECIMALS: u8 = 6;
const NATIVE_FAUCET_MAX_SUPPLY: u64 = 100_000_000_000_000_000;

/// Token metadata of the TST genesis faucet tests mint from.
const TST_FAUCET_DECIMALS: u8 = 12;
const TST_FAUCET_MAX_SUPPLY: u64 = 1_000_000_000_000;

/// Balance, in base units of the native fee asset, held by every genesis account that transacts.
const GENESIS_ACCOUNT_FEE_BALANCE: u64 = 1_000_000_000;

/// Writes the genesis fixtures into `output_dir` so the node can be bootstrapped with
/// `miden-validator bootstrap --genesis-config-file <output_dir>/genesis.toml`.
///
/// This emits the TST genesis faucet (written with its secret key), the test faucets, and the
/// `too_many_assets` account as `.mac` files referenced by `[[account]]` entries in
/// `genesis.toml`, which the node loads verbatim.
///
/// The native fee faucet is generated here and pointed at by `native_faucet` rather than left to
/// the node, so its ID is known before the remaining accounts are serialized. A vault entry can
/// only reference a faucet whose ID already exists, which is what lets those accounts be seeded.
///
/// `num_funder_wallets` declares that many `[[wallet]]` entries holding [`FUNDER_WALLET_BALANCE`].
/// The node writes each to its accounts directory as `wallet_<index>.mac`, secret key included.
///
/// The agglayer genesis accounts (bridge admin, GER manager, bridge, and faucet) are emitted too,
/// and integration tests load their `.mac` files via the `AGGLAYER_ACCOUNTS_DIR` env var. They are
/// always present because the bridge and faucet are network accounts, which no client transaction
/// can deploy, so a test cannot create them at runtime.
pub fn write_genesis_config(
    output_dir: &Path,
    verification_base_fee: u32,
    num_funder_wallets: u32,
) -> Result<()> {
    std::fs::create_dir_all(output_dir).with_context(|| {
        format!("failed to create genesis output directory {}", output_dir.display())
    })?;

    let mut account_files = Vec::new();

    // Generated before anything else so that the accounts below can hold its asset. The faucet
    // commits to its operator's ID, so the operator is built first, and both get their balance
    // only once the faucet's ID exists.
    let (operator, operator_secret) =
        generate_faucet_operator().context("failed to create the native faucet operator")?;
    let native_faucet =
        generate_native_faucet(operator.id()).context("failed to create the native fee faucet")?;
    let fee_balance: Asset =
        FungibleAsset::new(native_faucet.id(), GENESIS_ACCOUNT_FEE_BALANCE)?.into();
    AccountFile::new(into_genesis_account(native_faucet, fee_balance)?, vec![])
        .write(output_dir.join(NATIVE_FAUCET_FILE))
        .with_context(|| format!("failed to write {NATIVE_FAUCET_FILE}"))?;
    AccountFile::new(into_genesis_account(operator, fee_balance)?, vec![operator_secret])
        .write(output_dir.join(FAUCET_OPERATOR_FILE))
        .with_context(|| format!("failed to write {FAUCET_OPERATOR_FILE}"))?;
    account_files.push(FAUCET_OPERATOR_FILE.to_string());

    // Genesis faucet (TST), with its secret key so it can sign minting transactions and the fee
    // balance those settle from.
    let (tst_faucet, tst_secret) =
        generate_faucet("TST", TST_FAUCET_DECIMALS, TST_FAUCET_MAX_SUPPLY)
            .context("failed to create genesis faucet account")?;
    AccountFile::new(into_genesis_account(tst_faucet, fee_balance)?, vec![tst_secret])
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
    let agglayer_accounts = agglayer::create_agglayer_genesis_accounts(fee_balance)
        .context("failed to create agglayer genesis accounts")?;
    for (file_name, account_file) in agglayer_accounts {
        account_file
            .write(output_dir.join(file_name))
            .with_context(|| format!("failed to write {file_name}"))?;
        account_files.push(file_name.to_string());
    }

    let timestamp: u32 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("current timestamp should be greater than unix epoch")
        .as_secs()
        .try_into()
        .expect("timestamp should fit into u32");

    // The validator set is not part of this config: `miden-validator genesis` takes the set's
    // public keys on the command line, and `start-test-node.sh` generates the key-pair it passes
    // there alongside the matching signing key.
    let config = GenesisConfig {
        version: 1,
        timestamp,
        native_faucet: NATIVE_FAUCET_FILE.to_string(),
        fee_parameters: FeeParametersEntry { verification_base_fee },
        accounts: account_files.into_iter().map(|path| AccountEntry { path }).collect(),
        wallets: (0..num_funder_wallets)
            .map(|_| WalletEntry {
                account_type: "public".to_string(),
                assets: vec![AssetEntry {
                    amount: FUNDER_WALLET_BALANCE,
                    symbol: NATIVE_FAUCET_SYMBOL.to_string(),
                }],
            })
            .collect(),
    };

    let toml = toml::to_string(&config).context("failed to serialize genesis.toml")?;
    std::fs::write(output_dir.join("genesis.toml"), toml)
        .with_context(|| "failed to write genesis.toml")?;

    Ok(())
}

// GENESIS CONFIG
// ================================================================================================

/// The `genesis.toml` the node bootstraps from.
///
/// Field order is the serialized order, and TOML requires a table's own values before any nested
/// table, so the scalars have to stay above `fee_parameters` and the two arrays of tables.
#[derive(Serialize)]
struct GenesisConfig {
    version: u32,
    timestamp: u32,
    /// File name of the faucet whose asset the chain charges fees in.
    native_faucet: String,
    fee_parameters: FeeParametersEntry,
    /// Rendered as `[[account]]` entries, each naming a `.mac` file the node loads verbatim.
    #[serde(rename = "account")]
    accounts: Vec<AccountEntry>,
    /// Rendered as `[[wallet]]` entries the node creates and writes out as `wallet_<index>.mac`.
    #[serde(rename = "wallet")]
    wallets: Vec<WalletEntry>,
}

#[derive(Serialize)]
struct FeeParametersEntry {
    verification_base_fee: u32,
}

#[derive(Serialize)]
struct AccountEntry {
    path: String,
}

#[derive(Serialize)]
struct WalletEntry {
    account_type: String,
    assets: Vec<AssetEntry>,
}

/// A balance the node gives a generated wallet, naming the faucet by token symbol.
#[derive(Serialize)]
struct AssetEntry {
    amount: u64,
    symbol: String,
}

// GENESIS ACCOUNTS
// ================================================================================================

/// Builds a public singlesig fungible faucet, with the secret key that signs for it.
fn generate_faucet(
    symbol: &str,
    decimals: u8,
    max_supply: u64,
) -> anyhow::Result<(Account, AuthSecretKey)> {
    let mut rng = ChaCha20Rng::from_seed(random());
    let secret = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut rng);

    let symbol = TokenSymbol::try_from(symbol).expect("faucet symbol should be valid");
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(decimals)
        .max_supply(AssetAmount::new(max_supply)?)
        .build()?;
    let account = create_singlesig_user_fungible_faucet(
        rng.random(),
        faucet,
        AuthSingleSig::new(Approver::new(
            secret.public_key().to_commitment(),
            AuthScheme::Falcon512Poseidon2,
        )),
        allow_all_policy_manager(),
        AccountType::Public,
    )?;

    Ok((account, secret))
}

/// Builds the public wallet owning the native fee faucet, with the key that signs for it.
fn generate_faucet_operator() -> anyhow::Result<(Account, AuthSecretKey)> {
    let mut rng = ChaCha20Rng::from_seed(random());
    let secret = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut rng);
    let approver =
        Approver::new(secret.public_key().to_commitment(), AuthScheme::Falcon512Poseidon2);
    let operator = create_basic_wallet(rng.random(), approver, AccountType::Public)?;

    Ok((operator, secret))
}

/// Builds the native fee faucet as a network account owned by `operator_id`, mirroring the faucet
/// the node generates when a genesis leaves `native_faucet` unset.
fn generate_native_faucet(operator_id: AccountId) -> anyhow::Result<Account> {
    let mut rng = ChaCha20Rng::from_seed(random());

    let symbol =
        TokenSymbol::try_from(NATIVE_FAUCET_SYMBOL).expect("faucet symbol should be valid");
    let name = TokenName::new(&symbol.to_string()).expect("token symbol is a valid token name");
    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(NATIVE_FAUCET_DECIMALS)
        .max_supply(AssetAmount::new(NATIVE_FAUCET_MAX_SUPPLY)?)
        .build()?;

    let token_policies = TokenPolicyManager::builder()
        .active_mint_policy(MintPolicy::owner_only())
        .active_burn_policy(BurnPolicy::allow_all())
        .active_send_policy(TransferPolicy::allow_all())
        .active_receive_policy(TransferPolicy::allow_all())
        .build();

    let fee_policy = BasicConstantFeePolicy::new()
        .with_fees([
            (MintNote::script_root(), AssetAmount::ZERO),
            (BurnNote::script_root(), AssetAmount::ZERO),
        ])
        .into();
    let fee_policies = FeePolicyManager::builder()
        .fee_faucet_id(operator_id)
        .active_fee_policy(fee_policy)
        .build();

    let faucet = create_network_fungible_faucet(
        rng.random(),
        faucet,
        AccessControl::Ownable2Step { owner: operator_id },
        token_policies,
        fee_policies,
    )?;

    Ok(faucet)
}

/// Marks `account` as deployed at genesis and gives it a balance of the native fee asset.
///
/// A nonce of zero marks an undeployed account, and genesis deploys without transactions, so the
/// nonce is bumped by hand. The balance is what lets the account pay for its own transactions.
pub(crate) fn into_genesis_account(mut account: Account, fee_balance: Asset) -> Result<Account> {
    account
        .vault_mut()
        .add_asset(fee_balance)
        .context("failed to seed an account's native fee balance")?;
    account
        .set_nonce(ONE)
        .context("failed to mark an account as deployed at genesis")?;

    Ok(account)
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
/// `AccountStorageMapDetails::MAX_RETURN_ENTRIES` limit defined in the node, so the slot comes
/// back as `StorageMapEntries::LimitExceeded` during testing.
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

    let auth_component = AuthSingleSig::new(Approver::new(
        secret.public_key().to_commitment(),
        AuthScheme::Falcon512Poseidon2,
    ));

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
        BasicWallet::code().as_package().clone(),
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
        .with_component(AuthSingleSig::new(Approver::new(
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

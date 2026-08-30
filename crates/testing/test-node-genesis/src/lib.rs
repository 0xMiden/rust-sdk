//! Generates the genesis fixtures used to bootstrap a testing node from the standalone Miden node
//! executables. The accounts only depend on `miden-protocol`/`miden-standards`, so the generated
//! configuration is independent of the node's own crates.

pub mod agglayer;

use std::env::VarError;
use std::fmt::Write as _;
use std::num::NonZeroU32;
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
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

// GENESIS CONFIG GENERATION
// ================================================================================================

/// Env var setting `verification_base_fee` in the generated config, in the fee asset's smallest
/// denomination. [`verification_base_fee_from_env`] gives the rules for reading it.
pub const VERIFICATION_BASE_FEE_VAR: &str = "MIDEN_TEST_NODE_VERIFICATION_BASE_FEE";

/// Number of MIDEN-funded wallets emitted when the chain charges a fee.
///
/// Two so that two tests funding accounts at the same time need not serialise on one funder's
/// nonce, sized to `.config/nextest.toml` capping the `integration` test group at two threads.
/// That bound holds for `make integration-test`, not for `make integration-test-binary`, whose
/// runner defaults `--jobs` to the CPU count and would oversubscribe the pool; pass `--jobs 2`
/// there. Nothing assigns a funder to a test yet, so two only removes contention once a consumer
/// partitions them.
pub const FUNDER_COUNT: usize = 2;

/// MIDEN each funder holds, in the asset's smallest denomination. The native faucet has 6 decimals,
/// so this is 100,000 MIDEN. The node documents the field as full token units but passes it to
/// `FungibleAsset::new` unscaled, so the smallest denomination is what it actually credits.
///
/// A transaction's fee is `verification_base_fee` times `ilog2(cycles) + 1`, which the kernel caps
/// at 30, so this balance pays for `FUNDER_MIDEN / (30 * base_fee)` transactions — on the order of
/// 6.6m at 500, the base fee the protocol's own fee tests use — and a base fee above ~3.3 billion
/// exceeds the whole balance in a single transaction. That figure counts only the fees a funder
/// pays for itself: the MIDEN it hands to a test account, which is the reason it exists, comes out
/// of the same balance, so a funding policy's payouts set the real headroom.
/// `.config/nextest.toml` retries a failing test twice, so a flaky test can spend three times its
/// share.
///
/// A drained funder surfaces as the kernel abort "amount of the asset in the vault is less than the
/// amount to remove", which does not name the funder — raise this constant rather than hunting the
/// balance. It was raised once already, for exactly that reason: at `5_000_000_000` the two funders
/// covered ~500 payouts of the wallet harness's `20_000_000`, which a night of repeated E2E sweeps
/// exhausted. The failure is indirect — the harness reports "could not send its fee funding from
/// any of N genesis funder(s)" and the suite then fails on an unfunded account, which reads like a
/// product bug rather than an empty till.
///
/// At `100_000_000_000` the pair covers ~`10_000` such payouts, i.e. a few hundred full sweeps.
/// That is far below `AssetAmount::MAX` (2^63 - 2^31), so the headroom costs nothing: the balance
/// only has to exceed what a run spends, and nothing scales with it.
const FUNDER_MIDEN: u64 = 100_000_000_000;

/// Genesis faucet file name. Carries the secret key so the operator/tests can mint TST.
pub const GENESIS_FAUCET_FILE: &str = "tst_faucet.mac";

/// Writes the genesis fixtures into `output_dir` so a genesis block can be built by pointing
/// `miden-validator genesis --config` at `<output_dir>/genesis.toml`. That command also needs a
/// block directory, an accounts directory, and the validator's key; `scripts/start-test-node.sh`
/// has the full invocation.
///
/// This emits the TST genesis faucet (written with its secret key), the test faucets, and the
/// `too_many_assets` account as `.mac` files referenced by `[[account]]` entries in
/// `genesis.toml`, which the node loads verbatim.
///
/// The native faucet is left unset, so the node generates the default `MIDEN` faucet. When
/// `verification_base_fee` is `Some`, fees are charged in that faucet's asset, and the config
/// additionally carries [`FUNDER_COUNT`] MIDEN-funded funder wallets;
/// [`verification_base_fee_from_env`] resolves the value the harness passes here.
///
/// When `include_agglayer` is set, the agglayer genesis accounts (bridge admin, GER manager,
/// bridge, and faucet) are also emitted and included in genesis; integration tests load their
/// `.mac` files via the `AGGLAYER_ACCOUNTS_DIR` env var.
pub fn write_genesis_config(
    output_dir: &Path,
    include_agglayer: bool,
    verification_base_fee: Option<NonZeroU32>,
) -> Result<()> {
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

    let toml = render_genesis_toml(timestamp, verification_base_fee, &account_files);

    std::fs::write(output_dir.join("genesis.toml"), toml)
        .with_context(|| format!("failed to write genesis.toml in {}", output_dir.display()))?;

    Ok(())
}

/// Reads [`VERIFICATION_BASE_FEE_VAR`], mapping both an unset variable and an explicit `0` to
/// `None`. Collapsing the two here keeps "charges a fee" a single decision: everything downstream
/// reads it off the type rather than re-testing the number.
///
/// A value that is present but unusable is an error rather than a fallback to zero: silently
/// bringing up a fee-free chain when a fee was asked for would make fee tests pass for the wrong
/// reason.
pub fn verification_base_fee_from_env() -> Result<Option<NonZeroU32>> {
    base_fee_from_var(std::env::var(VERIFICATION_BASE_FEE_VAR))
}

/// Applies the rules above to what [`std::env::var`] returned. Taking the lookup's result rather
/// than reading the variable itself keeps every rule testable without mutating the process
/// environment, which tests share.
fn base_fee_from_var(value: Result<String, VarError>) -> Result<Option<NonZeroU32>> {
    let base_fee = match value {
        Ok(value) => value
            .parse()
            .with_context(|| format!("{VERIFICATION_BASE_FEE_VAR} must be a u32, got {value:?}"))?,
        Err(VarError::NotPresent) => 0,
        Err(err @ VarError::NotUnicode(_)) => {
            return Err(err).with_context(|| format!("failed to read {VERIFICATION_BASE_FEE_VAR}"));
        },
    };

    Ok(NonZeroU32::new(base_fee))
}

/// Renders the `genesis.toml` manifest that `miden-validator genesis` consumes, referencing each
/// `.mac` file in `account_files` by name.
///
/// The validator set is not part of this config: `miden-validator genesis` takes the set's public
/// keys on the command line, and `start-test-node.sh` generates the key-pair it passes there
/// alongside the matching signing key.
fn render_genesis_toml(
    timestamp: u32,
    verification_base_fee: Option<NonZeroU32>,
    account_files: &[String],
) -> String {
    let base_fee = verification_base_fee.map_or(0, NonZeroU32::get);
    let mut toml = format!(
        "version = 1\ntimestamp = {timestamp}\n\n\
         [fee_parameters]\nverification_base_fee = {base_fee}\n"
    );

    // The native MIDEN faucet is a keyless network account, so it cannot be minted from locally,
    // and `[[account]]` entries cannot be pre-funded here because the faucet's id is assigned only
    // when the node builds genesis. The node also generates an operator account that owns that
    // faucet, but it is the node's account for the node's purpose: this harness neither exports
    // nor spends it, and does not depend on whether a given node version leaves its vault empty or
    // pre-funds it. A genesis wallet is therefore the spendable source of the fee asset this
    // harness owns outright, and it is public so a client that did not create it can read its
    // state — which is what makes it usable as a shared funder.
    if verification_base_fee.is_some() {
        for _ in 0..FUNDER_COUNT {
            write!(
                toml,
                "\n[[wallet]]\naccount_type = \"public\"\nassets = [{{ amount = {FUNDER_MIDEN}, \
                 symbol = \"MIDEN\" }}]\n"
            )
            .expect("writing to a String cannot fail");
        }
    }

    for file_name in account_files {
        write!(toml, "\n[[account]]\npath = \"{file_name}\"\n")
            .expect("writing to a String cannot fail");
    }

    toml
}

// GENESIS ACCOUNTS
// ================================================================================================

fn generate_genesis_account() -> anyhow::Result<AccountFile> {
    let mut rng = ChaCha20Rng::from_seed(random());
    let secret = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut rng);

    let auth_component = AuthSingleSig::new(Approver::new(
        secret.public_key().to_commitment(),
        AuthScheme::Falcon512Poseidon2,
    ));

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

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const ACCOUNT_FILES: [&str; 2] = ["tst_faucet.mac", "test_account_0000.mac"];
    const TIMESTAMP: u32 = 1_717_344_256;

    /// The files `include_agglayer` adds, spelled out rather than read off `agglayer`'s constants
    /// so that renaming one is caught here instead of silently agreeing with itself.
    const AGGLAYER_FILES: [&str; 4] =
        ["agglayer_faucet.mac", "bridge.mac", "bridge_admin.mac", "ger_manager.mac"];

    /// Renders a manifest and parses it back, which asserts it is syntactically valid TOML — the
    /// property hand-written TOML loses most easily. That the field names match the node's schema
    /// can only be checked by bootstrapping the pinned node against the result.
    fn render_and_parse(verification_base_fee: u32) -> toml::Table {
        let account_files = ACCOUNT_FILES.map(String::from);
        let rendered =
            render_genesis_toml(TIMESTAMP, NonZeroU32::new(verification_base_fee), &account_files);

        rendered.parse().expect("rendered genesis config should be valid TOML")
    }

    fn account_paths(config: &toml::Table) -> Vec<&str> {
        config["account"]
            .as_array()
            .expect("accounts should be an array of tables")
            .iter()
            .map(|account| account["path"].as_str().expect("a path should be a string"))
            .collect()
    }

    /// The node's top-level genesis config struct is `deny_unknown_fields`, so an unexpected
    /// top-level key is a bootstrap failure rather than something it ignores. The parsed table is
    /// sorted by key, which is not the emitted order and does not need to be: TOML tables are
    /// unordered.
    #[test]
    fn only_the_expected_top_level_keys_are_emitted() {
        let fee_free: Vec<_> = render_and_parse(0).keys().cloned().collect();
        assert_eq!(fee_free, ["account", "fee_parameters", "timestamp", "version"]);

        let fee_charging: Vec<_> = render_and_parse(500).keys().cloned().collect();
        assert_eq!(fee_charging, ["account", "fee_parameters", "timestamp", "version", "wallet"]);
    }

    #[test]
    fn the_header_carries_the_version_and_timestamp() {
        let config = render_and_parse(0);

        assert_eq!(config["version"].as_integer(), Some(1));
        assert_eq!(config["timestamp"].as_integer(), Some(i64::from(TIMESTAMP)));
    }

    #[test]
    fn no_funders_are_emitted_when_the_chain_charges_no_fee() {
        let config = render_and_parse(0);

        assert_eq!(config["fee_parameters"]["verification_base_fee"].as_integer(), Some(0));
        assert!(!config.contains_key("wallet"), "a chain charging nothing needs no funders");
    }

    /// A fee of one is the boundary the funder emission turns on at, and `u32::MAX` is the largest
    /// the node's field can hold; both have to survive rendering unchanged.
    #[test]
    fn the_smallest_and_largest_fees_are_rendered_verbatim() {
        let smallest = render_and_parse(1);
        assert_eq!(smallest["fee_parameters"]["verification_base_fee"].as_integer(), Some(1));
        assert_eq!(smallest["wallet"].as_array().map(Vec::len), Some(FUNDER_COUNT));

        let largest = render_and_parse(u32::MAX);
        assert_eq!(
            largest["fee_parameters"]["verification_base_fee"].as_integer(),
            Some(i64::from(u32::MAX))
        );
        assert_eq!(largest["wallet"].as_array().map(Vec::len), Some(FUNDER_COUNT));
    }

    #[test]
    fn funders_hold_miden_when_the_chain_charges_a_fee() {
        let config = render_and_parse(500);

        assert_eq!(config["fee_parameters"]["verification_base_fee"].as_integer(), Some(500));

        // The count and the balance are spelled out rather than read off the constants, because
        // the README and the CHANGELOG both state them: changing either has to be deliberate here
        // and in the prose.
        assert_eq!(FUNDER_COUNT, 2);

        let wallets = config["wallet"].as_array().expect("wallets should be an array of tables");
        assert_eq!(wallets.len(), FUNDER_COUNT, "one wallet per funder");
        for wallet in wallets {
            // Public, so a client that did not create the funder can still read its state.
            assert_eq!(wallet["account_type"].as_str(), Some("public"));

            let assets = wallet["assets"].as_array().expect("assets should be an array");
            assert_eq!(assets.len(), 1, "a funder holds the fee asset only");
            assert_eq!(assets[0]["symbol"].as_str(), Some("MIDEN"));
            assert_eq!(
                assets[0]["amount"].as_integer(),
                Some(i64::try_from(FUNDER_MIDEN).expect("FUNDER_MIDEN must fit in an i64")),
                "genesis must credit the funder exactly FUNDER_MIDEN"
            );
        }
    }

    /// The node loads accounts by the paths listed here, so a dropped entry would leave a `.mac`
    /// file on disk out of genesis entirely — including on the fee-charging path, where the funder
    /// wallets are emitted alongside them.
    #[test]
    fn every_account_file_is_referenced_in_order() {
        assert_eq!(account_paths(&render_and_parse(0)), ACCOUNT_FILES);
        assert_eq!(account_paths(&render_and_parse(500)), ACCOUNT_FILES);
    }

    /// Every `.mac` file `write_genesis_config` left in `output_dir`, sorted. The node loads only
    /// what `[[account]]` names, so a file here that the manifest does not reference is a fixture
    /// that silently never reaches the chain.
    fn written_mac_files(output_dir: &Path) -> Vec<String> {
        let mut names: Vec<_> = std::fs::read_dir(output_dir)
            .expect("the output directory should be readable")
            .map(|entry| entry.expect("each directory entry should be readable").file_name())
            .filter_map(|name| name.into_string().ok())
            .filter(|name| Path::new(name).extension().is_some_and(|ext| ext == "mac"))
            .collect();
        names.sort();
        names
    }

    /// Asserts that the manifest and `output_dir` name exactly the same `.mac` files and that every
    /// referenced path is a regular file, then returns those names sorted. Differences are reported
    /// by name: both sides run past a thousand entries, so comparing the vectors wholesale buries a
    /// single missing file in the dump.
    fn assert_manifest_matches_directory(config: &toml::Table, output_dir: &Path) -> Vec<String> {
        let mut referenced: Vec<String> =
            account_paths(config).into_iter().map(String::from).collect();
        referenced.sort();

        let duplicated: Vec<_> = referenced
            .windows(2)
            .filter(|pair| pair[0] == pair[1])
            .map(|pair| &pair[0])
            .collect();
        assert!(
            duplicated.is_empty(),
            "the manifest names a path more than once: {duplicated:?}"
        );

        // `is_file` rather than `exists`, so a directory named after a fixture cannot stand in for
        // the fixture the node will try to read.
        let not_a_file: Vec<_> =
            referenced.iter().filter(|path| !output_dir.join(path).is_file()).collect();
        assert!(not_a_file.is_empty(), "referenced but not a file on disk: {not_a_file:?}");

        let written = written_mac_files(output_dir);
        let only_in_manifest: Vec<_> =
            referenced.iter().filter(|name| !written.contains(name)).collect();
        let only_on_disk: Vec<_> =
            written.iter().filter(|name| !referenced.contains(name)).collect();
        assert!(
            only_in_manifest.is_empty() && only_on_disk.is_empty(),
            "the manifest and the directory have to name the same .mac files; \
             only in the manifest: {only_in_manifest:?}; only on disk: {only_on_disk:?}"
        );

        referenced
    }

    /// The fixtures every configuration has to carry: the TST genesis faucet, plus one file per
    /// test faucet and one for the `too_many_assets` account. Pinned here because the check above
    /// compares the manifest and the directory against each other, and so stays green if the whole
    /// suite disappears from both at once.
    ///
    /// The `test_account_*` files are pinned by name rather than counted, so a writer that changed
    /// their numbering while emitting the right number of them is caught.
    fn assert_core_fixtures_present(referenced: &[String]) {
        assert!(
            referenced.iter().any(|name| name == GENESIS_FAUCET_FILE),
            "{GENESIS_FAUCET_FILE} has to reach genesis; tests mint TST with the key it carries"
        );

        let faucets =
            usize::try_from(NUM_TEST_FAUCETS).expect("the test faucet count should fit in a usize");
        // Inclusive: the writer emits one file per test faucet and then the too_many_assets
        // account, which lands on index `faucets`.
        let expected: Vec<String> =
            (0..=faucets).map(|index| format!("test_account_{index:04}.mac")).collect();
        for name in &expected {
            assert!(
                referenced.contains(name),
                "{name} has to reach genesis: one .mac per test faucet, plus the too_many_assets \
                 account"
            );
        }

        let written = referenced.iter().filter(|name| name.starts_with("test_account_")).count();
        assert_eq!(
            written,
            expected.len(),
            "genesis carries a test_account_* file the writer does not name"
        );
    }

    /// Runs the entry point the harness actually calls and returns the manifest it wrote, having
    /// first checked everything that holds on every path: the manifest and the directory name the
    /// same files, and the core fixtures are among them.
    ///
    /// The four callers below cover all four corners of `(include_agglayer,
    /// verification_base_fee)`. Nothing in the signature keeps the two flags independent, so
    /// testing only the corners where they agree would let a build that coupled them pass; the two
    /// mixed corners are also the two commands the README tells people to run.
    fn write_and_check(
        include_agglayer: bool,
        verification_base_fee: Option<NonZeroU32>,
    ) -> (toml::Table, Vec<String>) {
        let output_dir = tempfile::tempdir().expect("a temporary directory should be available");
        write_genesis_config(output_dir.path(), include_agglayer, verification_base_fee)
            .expect("writing the genesis config should succeed");

        let written = std::fs::read_to_string(output_dir.path().join("genesis.toml"))
            .expect("genesis.toml should have been written");
        let config: toml::Table = written.parse().expect("the written config should be TOML");

        let referenced = assert_manifest_matches_directory(&config, output_dir.path());
        assert_core_fixtures_present(&referenced);

        (config, referenced)
    }

    /// Agglayer on, fee on. The fee the entry point is handed has to reach the file on disk.
    #[test]
    fn the_written_manifest_carries_the_fee_and_names_files_that_exist() {
        let (config, referenced) = write_and_check(true, NonZeroU32::new(500));

        assert_eq!(config["fee_parameters"]["verification_base_fee"].as_integer(), Some(500));
        assert_eq!(config["wallet"].as_array().map(Vec::len), Some(FUNDER_COUNT));

        for name in AGGLAYER_FILES {
            assert!(referenced.contains(&name.to_string()), "{name} should be in genesis");
        }
    }

    /// The default path, `make start-node`: no agglayer, no fee. The agglayer fixtures have to be
    /// absent from the manifest *and* from the directory, since a stray `.mac` file would be loaded
    /// by nothing. The funders only from the manifest: this crate never writes a funder `.mac`, the
    /// node does that into its own accounts directory. A stray `[[wallet]]` would put MIDEN on a
    /// chain that charges nothing for it.
    #[test]
    fn the_default_path_writes_neither_funders_nor_agglayer_accounts() {
        let (config, referenced) = write_and_check(false, None);

        assert_eq!(config["fee_parameters"]["verification_base_fee"].as_integer(), Some(0));
        assert!(!config.contains_key("wallet"), "a fee-free chain needs no funders");

        for name in AGGLAYER_FILES {
            assert!(!referenced.contains(&name.to_string()), "{name} should not be in genesis");
        }
    }

    /// `make start-node-agglayer`: agglayer on, fee off.
    #[test]
    fn the_agglayer_path_without_a_fee_emits_accounts_but_no_funders() {
        let (config, referenced) = write_and_check(true, None);

        assert_eq!(config["fee_parameters"]["verification_base_fee"].as_integer(), Some(0));
        assert!(!config.contains_key("wallet"), "a fee-free chain needs no funders");

        for name in AGGLAYER_FILES {
            assert!(referenced.contains(&name.to_string()), "{name} should be in genesis");
        }
    }

    /// `MIDEN_TEST_NODE_VERIFICATION_BASE_FEE=500 make start-node`: fee on, agglayer off. The
    /// documented way to ask for a fee-charging chain, and the only corner where funders have to
    /// appear without agglayer accounts alongside them.
    #[test]
    fn the_fee_path_without_agglayer_emits_funders_but_no_accounts() {
        let (config, referenced) = write_and_check(false, NonZeroU32::new(500));

        assert_eq!(config["fee_parameters"]["verification_base_fee"].as_integer(), Some(500));
        assert_eq!(config["wallet"].as_array().map(Vec::len), Some(FUNDER_COUNT));

        for name in AGGLAYER_FILES {
            assert!(!referenced.contains(&name.to_string()), "{name} should not be in genesis");
        }
    }

    /// [`FUNDER_COUNT`]'s rustdoc sizes two to the `integration` test group's thread cap, which
    /// lives in a file no compiler checks against this constant.
    ///
    /// The bound is `>=`, not equality: more funders than concurrent tests wastes a little genesis
    /// MIDEN and nothing else, and `.config/nextest.toml` says the cap is there only until the
    /// node-side contention it works around is fixed, so it may as easily fall as rise. Raising it
    /// without raising the pool is the hazard — once a consumer partitions the funders, that puts
    /// two tests on one funder's nonce.
    ///
    /// The override applying the cap is checked too, filter included, since deleting it or
    /// pointing it at another binary lifts the bound entirely while leaving `max-threads` sitting
    /// there to satisfy the assertion above.
    #[test]
    fn the_funder_count_covers_the_integration_thread_cap() {
        let nextest_config = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../..")
            .join(".config/nextest.toml");
        let config: toml::Table = std::fs::read_to_string(&nextest_config)
            .unwrap_or_else(|err| panic!("{} should be readable: {err}", nextest_config.display()))
            .parse()
            .expect("nextest.toml should be valid TOML");

        let max_threads = config
            .get("test-groups")
            .and_then(|groups| groups.get("integration"))
            .and_then(|group| group.get("max-threads"))
            .and_then(toml::Value::as_integer)
            .unwrap_or_else(|| {
                panic!(
                    "{} should cap max-threads for the integration test group, which is what \
                     FUNDER_COUNT is sized to",
                    nextest_config.display()
                )
            });

        let funders = i64::try_from(FUNDER_COUNT).expect("FUNDER_COUNT should fit in an i64");
        assert!(
            funders >= max_threads,
            "FUNDER_COUNT is {funders} but the integration group runs up to {max_threads} tests at \
             once; raise FUNDER_COUNT to match, or those tests will share a funder's nonce"
        );

        let cap_applies = config
            .get("profile")
            .and_then(|profiles| profiles.get("default"))
            .and_then(|profile| profile.get("overrides"))
            .and_then(toml::Value::as_array)
            .is_some_and(|overrides| {
                overrides.iter().any(|entry| {
                    entry.get("test-group").and_then(toml::Value::as_str) == Some("integration")
                        && entry
                            .get("filter")
                            .and_then(toml::Value::as_str)
                            .is_some_and(|filter| filter.contains("binary(integration)"))
                })
            });
        assert!(
            cap_applies,
            "no override puts the integration test binary in the integration group, so \
             max-threads bounds nothing and FUNDER_COUNT's sizing argument does not hold"
        );
    }

    /// Spelled out because five other places name this variable; renaming it is a contract change.
    #[test]
    fn the_variable_is_the_one_the_documentation_names() {
        assert_eq!(VERIFICATION_BASE_FEE_VAR, "MIDEN_TEST_NODE_VERIFICATION_BASE_FEE");
    }

    #[test]
    fn a_u32_base_fee_is_accepted() {
        let fee = |value: &str| base_fee_from_var(Ok(value.to_string())).unwrap();

        assert_eq!(fee("500"), NonZeroU32::new(500));
        assert_eq!(fee("4294967295"), NonZeroU32::new(u32::MAX));
    }

    /// Both ways of asking for a fee-free chain have to reach the same state, and neither may be
    /// confused with a fee of one.
    #[test]
    fn an_absent_variable_and_an_explicit_zero_both_mean_no_fee() {
        assert_eq!(base_fee_from_var(Err(VarError::NotPresent)).unwrap(), None);
        assert_eq!(base_fee_from_var(Ok("0".to_string())).unwrap(), None);
    }

    #[test]
    fn a_base_fee_that_is_not_a_u32_is_rejected() {
        for value in ["", " ", "-1", "1.0", "1_000", "0x10", "4294967296", "true"] {
            let err = match base_fee_from_var(Ok(value.to_string())) {
                Ok(parsed) => panic!("{value:?} should not parse as a base fee, got {parsed:?}"),
                Err(err) => err.to_string(),
            };
            assert!(err.contains(VERIFICATION_BASE_FEE_VAR), "should name the variable: {err}");
            assert!(err.contains(&format!("{value:?}")), "should quote the value: {err}");
        }
    }

    /// A value that is not UTF-8 cannot be a fee, and erring beats reading it as "no fee" for the
    /// same reason a malformed one does.
    #[cfg(unix)]
    #[test]
    fn a_base_fee_that_is_not_unicode_is_rejected() {
        use std::os::unix::ffi::OsStringExt as _;

        let not_unicode = std::ffi::OsString::from_vec(vec![0xff]);
        let err = base_fee_from_var(Err(VarError::NotUnicode(not_unicode)))
            .expect_err("a non-Unicode value should not be read as a fee");

        assert!(
            err.to_string().contains(VERIFICATION_BASE_FEE_VAR),
            "should name the variable: {err}"
        );
    }
}

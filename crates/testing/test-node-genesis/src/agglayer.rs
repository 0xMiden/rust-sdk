//! Builds the agglayer genesis accounts (bridge admin, GER manager, bridge, faucet) included in
//! the genesis configuration when agglayer support is requested.

use ::rand::{RngExt, random};
use anyhow::{Context, Result};
use miden_agglayer::{create_agglayer_faucet, create_bridge_account};
use miden_protocol::account::auth::{AuthScheme, AuthSecretKey};
use miden_protocol::account::{
    Account,
    AccountBuilder,
    AccountComponent,
    AccountComponentMetadata,
    AccountFile,
    AccountType,
};
use miden_protocol::{Felt, ONE, Word};
use miden_standards::account::auth::{Approver, AuthSingleSig};
use miden_standards::account::wallets::BasicWallet;
use rand_chacha::ChaCha20Rng;
use rand_chacha::rand_core::SeedableRng;

/// File names for agglayer genesis account exports.
pub const BRIDGE_ADMIN_ACCOUNT_FILE: &str = "bridge_admin.mac";
pub const GER_MANAGER_ACCOUNT_FILE: &str = "ger_manager.mac";
pub const BRIDGE_ACCOUNT_FILE: &str = "bridge.mac";
pub const AGGLAYER_FAUCET_ACCOUNT_FILE: &str = "agglayer_faucet.mac";

/// Account files to include in genesis and save to disk (account + secret keys).
/// Each entry is (filename, `AccountFile`).
pub type AgglayerGenesisAccounts = Vec<(&'static str, AccountFile)>;

/// Creates all agglayer genesis accounts:
/// 1. Bridge Admin - public wallet with Falcon512 auth
/// 2. GER Manager - public wallet with Falcon512 auth
/// 3. Bridge - `AuthNetworkAccount` network account
/// 4. Faucet - `AuthNetworkAccount` network account for bridged tokens
///
/// All accounts have their nonce set to ONE (genesis convention), i.e. they are deployed.
///
/// In protocol 0.15 the bridge and faucet use `AuthNetworkAccount`, which rejects any
/// client-submitted transaction (the auth procedure forbids tx scripts, and the miden-client
/// always attaches one). They therefore cannot be deployed by a client transaction; they must be
/// deployed at genesis (here). The bridge is left **unconfigured** - the faucet is registered at
/// test time by submitting a `CONFIG_AGG_BRIDGE` note, which the node processes as a network
/// transaction (the only path allowed to mutate an `AuthNetworkAccount`). This keeps the genesis
/// faucet/bridge state consistent with the foundry-generated CLAIM leaf the test uses.
pub fn create_agglayer_genesis_accounts() -> Result<AgglayerGenesisAccounts> {
    let mut rng = ChaCha20Rng::from_seed(random());

    // 1. Create Bridge Admin
    let admin_secret = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut rng);
    let admin_account = build_wallet_account(&mut rng, &admin_secret)
        .context("failed to create bridge admin account")?;
    let admin_account = set_nonce_to_one(admin_account);

    // 2. Create GER Manager
    let ger_secret = AuthSecretKey::new_falcon512_poseidon2_with_rng(&mut rng);
    let ger_account = build_wallet_account(&mut rng, &ger_secret)
        .context("failed to create GER manager account")?;
    let ger_account = set_nonce_to_one(ger_account);

    // 3. Create and deploy the Bridge account (unconfigured; configured at test time).
    let bridge_seed: Word = rng.random::<[u32; 4]>().map(Felt::from).into();
    let bridge =
        create_bridge_account(bridge_seed, admin_account.id(), ger_account.id(), ger_account.id());
    let bridge = set_nonce_to_one(bridge);

    // 4. Create and deploy the Faucet. In protocol 0.15 the faucet no longer stores conversion
    // metadata (origin token address, network, scale, metadata hash); that data lives on the
    // bridge's `faucet_metadata_map` and is written by the CONFIG_AGG_BRIDGE note at test time.
    let faucet_seed: Word = rng.random::<[u32; 4]>().map(Felt::from).into();
    let faucet =
        create_agglayer_faucet(faucet_seed, "AGG", 12, Felt::from(1_000_000_000u32), bridge.id());
    let faucet = set_nonce_to_one(faucet);

    let admin_file = AccountFile::new(admin_account, vec![admin_secret]);
    let ger_file = AccountFile::new(ger_account, vec![ger_secret]);
    let bridge_file = AccountFile::new(bridge, vec![]);
    let faucet_file = AccountFile::new(faucet, vec![]);

    Ok(vec![
        (BRIDGE_ADMIN_ACCOUNT_FILE, admin_file),
        (GER_MANAGER_ACCOUNT_FILE, ger_file),
        (BRIDGE_ACCOUNT_FILE, bridge_file),
        (AGGLAYER_FAUCET_ACCOUNT_FILE, faucet_file),
    ])
}

fn set_nonce_to_one(account: Account) -> Account {
    let (id, vault, storage, code, ..) = account.into_parts();
    Account::new_unchecked(id, vault, storage, code, ONE, None)
}

fn build_wallet_account(rng: &mut ChaCha20Rng, secret: &AuthSecretKey) -> Result<Account> {
    let seed: [u8; 32] = rng.random();

    let acc_component = AccountComponent::new(
        BasicWallet::code().as_library().clone(),
        vec![],
        AccountComponentMetadata::new("miden::testing::basic_wallet"),
    )
    .context("failed to create wallet component")?;

    let account = AccountBuilder::new(seed)
        .with_auth_component(AuthSingleSig::new(Approver::new(
            secret.public_key().to_commitment(),
            AuthScheme::Falcon512Poseidon2,
        )))
        .with_component(acc_component)
        .account_type(AccountType::Public)
        .build()
        .context("failed to build wallet account")?;

    Ok(account)
}

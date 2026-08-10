use std::path::PathBuf;
use std::{env, fs};

use miden_client::account::component::{
    AccountComponentMetadata,
    AuthGuardedMultisig,
    AuthMultisig,
    AuthNetworkAccount,
    AuthSingleSig,
    BasicWallet,
    FungibleFaucet,
    MIDEN_PACKAGE_EXTENSION,
    NoAuth,
    NonFungibleFaucet,
};
use miden_client::utils::Serializable;
use miden_client::vm::{Package, Section, SectionId, TargetType};

const PACKAGE_DIR: &str = "packages";

fn main() {
    // Basic wallet (no storage schema)
    let basic_wallet_metadata = BasicWallet::component_metadata();
    build_package("basic-wallet", BasicWallet::code().as_package(), &basic_wallet_metadata, None);

    // Basic fungible faucet
    let basic_faucet_metadata = FungibleFaucet::component_metadata();
    build_package(
        "basic-fungible-faucet",
        FungibleFaucet::code().as_package(),
        &basic_faucet_metadata,
        None,
    );

    // Basic non-fungible faucet
    let non_fungible_faucet_metadata = NonFungibleFaucet::component_metadata();
    build_package(
        "basic-non-fungible-faucet",
        NonFungibleFaucet::code().as_package(),
        &non_fungible_faucet_metadata,
        None,
    );

    // Basic auth (singlesig - supports both RPO Falcon and ECDSA)
    let singlesig_metadata = AuthSingleSig::component_metadata();

    build_package(
        "basic-auth",
        AuthSingleSig::code().as_package(),
        &singlesig_metadata,
        Some("auth"),
    );

    // ECDSA auth (same component, different package name for discoverability)
    build_package(
        "ecdsa-auth",
        AuthSingleSig::code().as_package(),
        &singlesig_metadata,
        Some("auth"),
    );

    // No authentication component. Nonce is incremented on first transaction and when the account
    // state is changed. Provides no cryptographic authentication.
    let no_auth_metadata = NoAuth::component_metadata();
    build_package("no-auth", NoAuth::code().as_package(), &no_auth_metadata, Some("auth"));

    // Multisig auth
    let multisig_metadata = AuthMultisig::component_metadata();
    build_package(
        "multisig-auth",
        AuthMultisig::code().as_package(),
        &multisig_metadata,
        Some("auth"),
    );

    // Guarded multisig auth
    let guarded_multisig_metadata = AuthGuardedMultisig::component_metadata();
    build_package(
        "guarded-multisig-auth",
        AuthGuardedMultisig::code().as_package(),
        &guarded_multisig_metadata,
        Some("auth"),
    );

    // Network account auth
    let network_account_metadata = AuthNetworkAccount::component_metadata();
    build_package(
        "network-account-auth",
        AuthNetworkAccount::code().as_package(),
        &network_account_metadata,
        Some("auth"),
    );
}

/// Builds a package and stores it under `{OUT_DIR}/{PACKAGE_DIR}` or
/// `{OUT_DIR}/{PACKAGE_DIR}/{subdirectory}` if a subdirectory is provided.
pub fn build_package(
    package_name: &str,
    component_package: &Package,
    metadata: &AccountComponentMetadata,
    subdirectory: Option<&str>,
) {
    // The component's code is already a package carrying its MAST forest and exports, so it only
    // needs the component's identity and its metadata section on top.
    let mut package = component_package.clone();
    package.name = metadata.name().to_string().into();
    package.version = metadata.version().clone();
    package.kind = TargetType::AccountComponent;
    package.description = Some(metadata.description().to_string());
    package.sections =
        vec![Section::new(SectionId::ACCOUNT_COMPONENT_METADATA, metadata.to_bytes())];

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR environment variable not set");

    // Write the file
    let mut packages_out_dir = PathBuf::from(&out_dir).join(PACKAGE_DIR);
    if let Some(subdir) = subdirectory {
        packages_out_dir = packages_out_dir.join(subdir);
    }
    fs::create_dir_all(&packages_out_dir).expect("Failed to packages directory in OUT_DIR");

    let output_filename = format!("{package_name}.{MIDEN_PACKAGE_EXTENSION}");
    let output_file = packages_out_dir.join(&output_filename);

    fs::write(&output_file, package.to_bytes()).unwrap_or_else(|e| {
        panic!(
            "Failed to write Package {} to file {} in {}. Error: {}",
            package.name, output_filename, out_dir, e
        );
    });
}

use std::path::PathBuf;
use std::{env, fs};

use miden_client::account::component::{
    AccountComponentMetadata,
    AuthMultisig,
    AuthSingleSig,
    AuthSingleSigAcl,
    BasicWallet,
    FungibleFaucet,
    MIDEN_PACKAGE_EXTENSION,
    NoAuth,
};
use miden_client::assembly::Library;
use miden_client::utils::Serializable;
use miden_client::vm::{
    Package,
    PackageExport,
    ProcedureExport,
    QualifiedProcedureName,
    Section,
    SectionId,
    TargetType,
};

const PACKAGE_DIR: &str = "packages";

fn main() {
    // Basic wallet (no storage schema)
    let basic_wallet_metadata = BasicWallet::component_metadata();
    build_package("basic-wallet", BasicWallet::code().as_library(), &basic_wallet_metadata, None);

    // Basic fungible faucet
    let basic_faucet_metadata = FungibleFaucet::component_metadata();
    build_package(
        "basic-fungible-faucet",
        FungibleFaucet::code().as_library(),
        &basic_faucet_metadata,
        None,
    );

    // Basic auth (singlesig - supports both RPO Falcon and ECDSA)
    let singlesig_metadata = AuthSingleSig::component_metadata();

    build_package(
        "basic-auth",
        AuthSingleSig::code().as_library(),
        &singlesig_metadata,
        Some("auth"),
    );

    // ECDSA auth (same component, different package name for discoverability)
    build_package(
        "ecdsa-auth",
        AuthSingleSig::code().as_library(),
        &singlesig_metadata,
        Some("auth"),
    );

    // No authentication component. Nonce is incremented on first transaction and when the account
    // state is changed. Provides no cryptographic authentication.
    let no_auth_metadata = NoAuth::component_metadata();
    build_package("no-auth", NoAuth::code().as_library(), &no_auth_metadata, Some("auth"));

    // Multisig auth
    let multisig_metadata = AuthMultisig::component_metadata();
    build_package(
        "multisig-auth",
        AuthMultisig::code().as_library(),
        &multisig_metadata,
        Some("auth"),
    );

    // ACL auth
    let acl_metadata = AuthSingleSigAcl::component_metadata();
    build_package("acl-auth", AuthSingleSigAcl::code().as_library(), &acl_metadata, Some("auth"));
}

/// Builds a package and stores it under `{OUT_DIR}/{PACKAGE_DIR}` or
/// `{OUT_DIR}/{PACKAGE_DIR}/{subdirectory}` if a subdirectory is provided.
pub fn build_package(
    package_name: &str,
    library: &Library,
    metadata: &AccountComponentMetadata,
    subdirectory: Option<&str>,
) {
    // NOTE: Taken from the miden-compiler's build_package function:
    // https://github.com/0xMiden/compiler/blob/61ee77f57c07c197323728642f8feca972b24217/midenc-compile/src/stages/assemble.rs#L71-L88
    // Gather all of the procedure metadata for exports of this package
    let mut exports: Vec<PackageExport> = Vec::new();
    for module_info in library.module_infos() {
        for (_, proc_info) in module_info.procedures() {
            let name = QualifiedProcedureName::new(module_info.path(), proc_info.name.clone());
            let export = ProcedureExport {
                path: name.into_inner(),
                node: None,
                source_node: None,
                digest: proc_info.digest,
                signature: proc_info.signature.as_deref().cloned(),
                attributes: proc_info.attributes.clone(),
            };
            exports.push(PackageExport::Procedure(export));
        }
    }

    let mast = library.mast_forest().clone();

    let account_component_metadata_section =
        Section::new(SectionId::ACCOUNT_COMPONENT_METADATA, metadata.to_bytes());

    let mut package = Package::create(
        metadata.name().to_string().into(),
        metadata.version().clone(),
        TargetType::AccountComponent,
        mast,
        exports,
        [],
    )
    .expect("package creation failed");
    package.description = Some(metadata.description().to_string());
    package.sections = vec![account_component_metadata_section];

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

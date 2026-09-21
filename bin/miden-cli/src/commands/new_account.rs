use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use miden_client::Client;
use miden_client::account::component::{
    AccountComponent,
    AccountComponentMetadata,
    BurnPolicy,
    FungibleFaucet,
    InitStorageData,
    MIDEN_PACKAGE_EXTENSION,
    MintPolicy,
    TokenName,
    TokenPolicyManager,
};
use miden_client::account::{
    Account,
    AccountBuilder,
    AccountBuilderSchemaCommitmentExt,
    AccountType,
};
use miden_client::asset::{AssetAmount, TokenSymbol};
use miden_client::auth::{Approver, AuthSchemeId, AuthSecretKey, AuthSingleSig};
use miden_client::crypto::ecdsa_k256_keccak;
use miden_client::keystore::Keystore;
use miden_client::utils::{Deserializable, hex_to_bytes};
use miden_client::vm::{Package, TargetType};
use rand::Rng;
use serde::Deserialize;
use tracing::debug;

use crate::commands::account::set_default_account_if_unset;
use crate::config::CliConfig;
use crate::errors::CliError;
use crate::{CliKeyStore, client_binary_name};

// CLI TYPES
// ================================================================================================

/// Mirror enum for the protocol's public/private [`AccountType`].
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CliAccountType {
    Private,
    Public,
}

impl From<CliAccountType> for AccountType {
    fn from(cli_account_type: CliAccountType) -> Self {
        match cli_account_type {
            CliAccountType::Private => AccountType::Private,
            CliAccountType::Public => AccountType::Public,
        }
    }
}

// NEW WALLET
// ================================================================================================

/// Creates a new wallet account and store it locally.
///
/// A wallet account exposes functionality to sign transactions and manage asset transfers.
/// Additionally, more component templates can be added by specifying a list of component template
/// files.
#[derive(Debug, Parser, Clone)]
pub struct NewWalletCmd {
    /// Account type (`private` or `public`).
    #[arg(value_enum, short = 't', long = "account-type", default_value_t = CliAccountType::Private)]
    pub account_type: CliAccountType,
    /// Optional list of paths specifying additional components in the form of packages to add to
    /// the account.
    #[arg(short, long)]
    pub extra_packages: Vec<PathBuf>,
    /// Optional file path to a TOML file containing a list of key/values used for initializing
    /// storage. Each of these keys should map to the templated storage values within the passed
    /// list of component templates. The user will be prompted to provide values for any keys not
    /// present in the init storage data file.
    #[arg(short, long)]
    pub init_storage_data_path: Option<PathBuf>,
    /// Seed local-only state so the wallet can be created and used for execution without a node.
    /// Only available when built with the `testing` feature.
    #[cfg_attr(feature = "testing", arg(long, default_value_t = false))]
    #[cfg_attr(not(feature = "testing"), arg(skip = false))]
    pub offline: bool,
    /// Hex-encoded secp256k1 public key of an external signer (e.g. a Ledger device), in SEC1
    /// compressed (33-byte) or uncompressed (65-byte) form, `0x`-prefixed.
    ///
    /// The wallet is created with an ECDSA authentication component committing to this key. No
    /// secret key is generated or stored, so transactions must be signed by the external key
    /// holder. Cannot be combined with a package that contributes an auth component.
    #[arg(long, value_name = "HEX")]
    pub ecdsa_public_key: Option<String>,
}

impl NewWalletCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
        keystore: CliKeyStore,
    ) -> Result<(), CliError> {
        let package_paths: Vec<PathBuf> = [PathBuf::from("basic-wallet")]
            .into_iter()
            .chain(self.extra_packages.clone())
            .collect();

        let new_account = create_client_account(
            &mut client,
            &keystore,
            self.account_type.into(),
            &package_paths,
            self.init_storage_data_path.clone(),
            self.offline,
            self.ecdsa_public_key.as_deref(),
        )
        .await?;

        println!("Successfully created new wallet.");
        println!(
            "To view account details execute {} account -s {}",
            client_binary_name().display(),
            new_account.id().to_hex()
        );

        set_default_account_if_unset(&mut client, new_account.id()).await?;

        Ok(())
    }
}

// NEW ACCOUNT
// ================================================================================================

/// Creates a new account and saves it locally.
///
/// An account may comprise one or more components, each with its own storage and distinct
/// functionality.
///
/// # Authentication Components
///
/// If a package with an authentication component is provided via `-p`, it will be used for the
/// account. Otherwise, a default `RpoFalcon512` authentication component will be added
/// automatically.
///
/// Each account can only have one authentication component. If multiple packages contain
/// authentication components, an error will be returned. By default, authentication-related
/// packages are located in the `auth` subdir in your packages directory.
///
/// # Examples
///
/// Create a regular account with default Falcon auth:
/// ```bash
/// miden-client new-account -p basic-wallet
/// ```
///
/// Create a public account with a custom auth component (e.g., NoAuth):
/// ```bash
/// miden-client new-account -t public -p auth/no-auth -p basic-wallet
/// ```
///
/// Create a fungible faucet account (faucet-ness is derived from the `FungibleFaucet` component
/// contributed by the package, so no extra flag is needed):
/// ```bash
/// miden-client new-account -p basic-fungible-faucet -i init_data.toml
/// ```
#[derive(Debug, Parser, Clone)]
pub struct NewAccountCmd {
    /// Account type (`private` or `public`).
    #[arg(value_enum, short = 't', long = "account-type", default_value_t = CliAccountType::Private)]
    pub account_type: CliAccountType,
    /// List of files specifying package files used to create account components for the account. If
    /// any package contributes a `FungibleFaucet` component, the resulting account is treated as a
    /// fungible faucet (and an implicit `TokenPolicyManager` is installed when not already
    /// provided).
    #[arg(short, long, required = true)]
    pub packages: Vec<PathBuf>,
    /// Optional file path to a TOML file containing a list of key/values used for initializing
    /// storage. Each of these keys should map to the templated storage values within the passed
    /// list of component templates. The user will be prompted to provide values for any keys not
    /// present in the init storage data file.
    #[arg(short, long)]
    pub init_storage_data_path: Option<PathBuf>,
    /// Seed local-only state so the account can be created and used for execution without a node.
    /// Only available when built with the `testing` feature.
    #[cfg_attr(feature = "testing", arg(long, default_value_t = false))]
    #[cfg_attr(not(feature = "testing"), arg(skip = false))]
    pub offline: bool,
    /// Hex-encoded secp256k1 public key of an external signer (e.g. a Ledger device), in SEC1
    /// compressed (33-byte) or uncompressed (65-byte) form, `0x`-prefixed.
    ///
    /// The account is created with an ECDSA authentication component committing to this key. No
    /// secret key is generated or stored, so transactions must be signed by the external key
    /// holder. Cannot be combined with a package that contributes an auth component.
    #[arg(long, value_name = "HEX")]
    pub ecdsa_public_key: Option<String>,
}

impl NewAccountCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
        keystore: CliKeyStore,
    ) -> Result<(), CliError> {
        let new_account = create_client_account(
            &mut client,
            &keystore,
            self.account_type.into(),
            &self.packages,
            self.init_storage_data_path.clone(),
            self.offline,
            self.ecdsa_public_key.as_deref(),
        )
        .await?;

        println!("Successfully created new account.");
        println!(
            "To view account details execute {} account -s {}",
            client_binary_name().display(),
            new_account.id().to_hex()
        );

        Ok(())
    }
}

// HELPERS
// ================================================================================================

/// Reads [[`miden_core::vm::Package`]]s from the given file paths.
///
/// A bare name resolves to a package in the configured package directory. The CLI writes those
/// packages itself, so they are read as trusted. A path with the `.masp` extension is used as is
/// and is read as untrusted, so its MAST forest is validated.
pub(crate) fn load_packages(
    cli_config: &CliConfig,
    package_paths: &[PathBuf],
) -> Result<Vec<Package>, CliError> {
    let mut packages = Vec::with_capacity(package_paths.len());

    let packages_dir = &cli_config.package_directory;
    for path in package_paths {
        // If a user passes in a file with the `.masp` file extension, then we leave the path as is;
        // since it probably is a full path (this is the case with cargo-miden for instance).
        let (path, trusted) = match path.extension() {
            None => {
                let path = path.with_extension(MIDEN_PACKAGE_EXTENSION);
                Ok((packages_dir.join(path), true))
            },
            Some(extension) => {
                if extension == OsStr::new(MIDEN_PACKAGE_EXTENSION) {
                    Ok((path.clone(), false))
                } else {
                    let error = std::io::Error::new(
                        std::io::ErrorKind::InvalidFilename,
                        format!(
                            "{} has an invalid file extension: '{}'. \
                            Expected: {MIDEN_PACKAGE_EXTENSION}",
                            path.display(),
                            extension.display()
                        ),
                    );
                    Err(CliError::AccountComponentError(
                        Box::new(error),
                        format!("refuesed to read {}", path.display()),
                    ))
                }
            },
        }?;

        let bytes = fs::read(&path).map_err(|e| {
            CliError::AccountComponentError(
                Box::new(e),
                format!("failed to read Package file from {}", path.display()),
            )
        })?;

        let package = if trusted {
            Package::read_from_bytes_trusted(&bytes)
        } else {
            Package::read_from_bytes(&bytes)
        }
        .map_err(|e| {
            CliError::AccountComponentError(
                Box::new(e),
                format!("failed to deserialize Package in {}", path.display()),
            )
        })?;

        packages.push(package);
    }

    Ok(packages)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FungibleFaucetMetadata {
    symbol: String,
    decimals: u8,
    max_supply: u64,
    #[serde(default)]
    name: String,
}

/// Builds a fully-populated [`FungibleFaucet`] [`AccountComponent`] from the user-supplied
/// `[fungible-faucet-metadata]` block.
///
/// `FungibleFaucet` embeds the token metadata and requires every storage slot to be initialized to
/// deploy the `basic-fungible-faucet` package. Rather than encode the schema's field-level layout
/// here, the component is built directly from the high-level metadata via the typed builder, which
/// produces the same code and storage layout the package would have.
fn build_fungible_faucet_component(
    metadata: &FungibleFaucetMetadata,
) -> Result<AccountComponent, CliError> {
    let symbol = TokenSymbol::new(&metadata.symbol).map_err(|err| {
        CliError::InvalidArgument(format!("invalid token symbol `{}`: {err}", metadata.symbol))
    })?;
    let name_input = if metadata.name.is_empty() {
        metadata.symbol.as_str()
    } else {
        metadata.name.as_str()
    };
    let name = TokenName::new(name_input).map_err(|err| {
        CliError::InvalidArgument(format!("invalid token name `{name_input}`: {err}"))
    })?;
    let max_supply = AssetAmount::new(metadata.max_supply).map_err(|err| {
        CliError::InvalidArgument(format!("invalid max_supply `{}`: {err}", metadata.max_supply))
    })?;

    let faucet = FungibleFaucet::builder()
        .name(name)
        .symbol(symbol)
        .decimals(metadata.decimals)
        .max_supply(max_supply)
        .build()
        .map_err(|err| {
            CliError::InvalidArgument(format!("failed to build fungible faucet metadata: {err}"))
        })?;

    Ok(faucet.into())
}

/// Removes any package whose component name matches the upstream `FungibleFaucet` from the list,
/// since we'll inject the equivalent component directly from the user-supplied
/// `[fungible-faucet-metadata]` instead of going through the package's prompt-driven init-data
/// path. (The package files are typically distributed as `basic-fungible-faucet.masp` but the
/// `Package.name` field stores the component's full canonical name from `FungibleFaucet::NAME`.)
fn drop_basic_fungible_faucet_packages(packages: &mut Vec<Package>) -> bool {
    let before = packages.len();
    packages.retain(|pkg| pkg.name != FungibleFaucet::NAME);
    packages.len() != before
}

/// Loads the initialization storage data from an optional TOML file. If None is passed, an empty
/// object is returned.
fn load_init_storage_data(
    path: Option<&PathBuf>,
) -> Result<(InitStorageData, Option<FungibleFaucetMetadata>), CliError> {
    let Some(path) = path else {
        return Ok((InitStorageData::default(), None));
    };

    let mut contents = String::new();
    File::open(path)
        .and_then(|mut f| f.read_to_string(&mut contents))
        .map_err(|err| {
            CliError::InitDataError(
                Box::new(err),
                format!("Failed to open init data  file {}", path.display()),
            )
        })?;

    let mut table: toml::Table = toml::from_str(&contents).map_err(|err| {
        CliError::InitDataError(
            Box::new(err),
            format!("Failed to parse init data file {} as TOML", path.display()),
        )
    })?;

    let faucet_metadata = table
        .remove("fungible-faucet-metadata")
        .map(FungibleFaucetMetadata::deserialize)
        .transpose()
        .map_err(|err| {
            CliError::InitDataError(
                Box::new(err),
                format!("Invalid `fungible-faucet-metadata` in init data file {}", path.display()),
            )
        })?;

    let stripped = toml::to_string(&table).map_err(|err| {
        CliError::InitDataError(
            Box::new(err),
            format!("Failed to re-serialize init data from file {}", path.display()),
        )
    })?;

    let init = InitStorageData::from_toml(&stripped).map_err(|err| {
        CliError::InitDataError(
            Box::new(err),
            format!("Failed to deserialize init data from file {}", path.display()),
        )
    })?;

    Ok((init, faucet_metadata))
}

/// Byte length of a SEC1-compressed secp256k1 public key (parity prefix plus x coordinate).
const ECDSA_COMPRESSED_KEY_BYTES: usize = 33;
/// Byte length of a SEC1-uncompressed secp256k1 public key (`0x04` prefix plus both coordinates).
const ECDSA_UNCOMPRESSED_KEY_BYTES: usize = 65;

/// SPKI (RFC 5280) ASN.1 DER header declaring an uncompressed secp256k1 EC public key. The 65-byte
/// SEC1 point follows these bytes directly. Layout:
///
/// ```text
/// 30 56           SEQUENCE (86 bytes)
///   30 10         SEQUENCE, AlgorithmIdentifier (16 bytes)
///     06 07 2a 86 48 ce 3d 02 01   OID 1.2.840.10045.2.1 (ecPublicKey)
///     06 05 2b 81 04 00 0a         OID 1.3.132.0.10 (secp256k1)
///   03 42 00      BIT STRING (66 bytes, no unused bits): the SEC1 point
/// ```
const SECP256K1_SPKI_HEADER: [u8; 23] = [
    0x30, 0x56, 0x30, 0x10, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x05, 0x2b,
    0x81, 0x04, 0x00, 0x0a, 0x03, 0x42, 0x00,
];

/// Parses a hex-encoded secp256k1 public key in SEC1 format.
///
/// Accepts the 33-byte compressed and the 65-byte uncompressed encoding (the form Ledger and other
/// Ethereum-style signers export), both with a mandatory `0x` prefix. The point is fully validated:
/// an uncompressed key whose coordinates do not lie on the curve is rejected.
fn invalid_ecdsa_key(err: impl core::fmt::Display) -> CliError {
    CliError::InvalidArgument(format!("invalid ECDSA public key: {err}"))
}

fn parse_ecdsa_public_key(encoded: &str) -> Result<ecdsa_k256_keccak::PublicKey, CliError> {
    let hex_digits = encoded.strip_prefix("0x").ok_or_else(|| {
        CliError::InvalidArgument(
            "ECDSA public key must use a 0x-prefixed hexadecimal encoding".to_string(),
        )
    })?;

    match hex_digits.len() {
        len if len == ECDSA_COMPRESSED_KEY_BYTES * 2 => {
            let bytes =
                hex_to_bytes::<ECDSA_COMPRESSED_KEY_BYTES>(encoded).map_err(invalid_ecdsa_key)?;
            ecdsa_k256_keccak::PublicKey::read_from_bytes(&bytes).map_err(invalid_ecdsa_key)
        },
        len if len == ECDSA_UNCOMPRESSED_KEY_BYTES * 2 => {
            let bytes =
                hex_to_bytes::<ECDSA_UNCOMPRESSED_KEY_BYTES>(encoded).map_err(invalid_ecdsa_key)?;
            // Wrapping the point in an SPKI document lets the DER constructor validate both
            // coordinates against the curve equation. Compressing the point locally instead would
            // drop the y coordinate and silently accept a corrupted key whose y parity happens to
            // match.
            let mut der = Vec::with_capacity(SECP256K1_SPKI_HEADER.len() + bytes.len());
            der.extend_from_slice(&SECP256K1_SPKI_HEADER);
            der.extend_from_slice(&bytes);
            ecdsa_k256_keccak::PublicKey::from_der(&der).map_err(invalid_ecdsa_key)
        },
        len => Err(CliError::InvalidArgument(format!(
            "unsupported ECDSA public key length: expected {} (compressed) or {} (uncompressed) \
            hexadecimal digits after the 0x prefix, got {len}",
            ECDSA_COMPRESSED_KEY_BYTES * 2,
            ECDSA_UNCOMPRESSED_KEY_BYTES * 2,
        ))),
    }
}

/// Returns `true` when the CLI should inject a default `TokenPolicyManager` for a fungible faucet
/// account built from package components.
///
/// Why this exists:
/// - Fungible faucets require a token policy manager (with mint and burn policies) in addition to
///   `BasicFungibleFaucet`.
/// - The CLI's built-in `basic-fungible-faucet` package only contributes the faucet component
///   itself; it does not include a `TokenPolicyManager`.
/// - Other faucet creation paths in this repo install a manager configured with `AllowAll` mint and
///   burn policies explicitly, so the CLI adds the same configuration implicitly here to keep
///   faucet creation consistent across paths.
///
/// What it does:
/// - triggers when `BasicFungibleFaucet` is present in the resulting components (this is also the
///   signal that says "this account is a faucet" — no separate flag needed),
/// - skips injection if a `TokenPolicyManager` component is already present so user-provided policy
///   configurations are not duplicated or overridden.
fn should_add_implicit_token_policy_manager(regular_components: &[AccountComponent]) -> bool {
    let has_basic_fungible_faucet = regular_components
        .iter()
        .any(|component| component.metadata().name() == FungibleFaucet::NAME);
    let has_token_policy_manager = regular_components
        .iter()
        .any(|component| component.metadata().name() == TokenPolicyManager::NAME);

    has_basic_fungible_faucet && !has_token_policy_manager
}

/// Helper function to create the seed, initialize the account builder, add the given components,
/// and build the account.
///
/// When `ecdsa_public_key` is given, an ECDSA auth component committing to that externally-held key
/// is added and no secret key is generated or stored. Otherwise, if no auth component is detected
/// in the packages, a Falcon-based auth component will be added.
async fn create_client_account<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    keystore: &CliKeyStore,
    account_type: AccountType,
    package_paths: &[PathBuf],
    init_storage_data_path: Option<PathBuf>,
    offline: bool,
    ecdsa_public_key: Option<&str>,
) -> Result<Account, CliError> {
    if package_paths.is_empty() {
        return Err(CliError::InvalidArgument(
            "Account must contain at least one component".to_string(),
        ));
    }

    let external_key = ecdsa_public_key.map(parse_ecdsa_public_key).transpose()?;

    // Load the component templates and initialization storage data.
    let cli_config = CliConfig::load()?;
    debug!("Loading packages...");
    let packages = load_packages(&cli_config, package_paths)?;
    debug!("Loaded {} packages", packages.len());
    debug!("Loading initialization storage data...");
    let (init_storage_data, faucet_metadata) =
        load_init_storage_data(init_storage_data_path.as_ref())?;
    debug!("Loaded initialization storage data");

    // `FungibleFaucet` requires every storage slot to be initialized. When the user provides a
    // `[fungible-faucet-metadata]` TOML block, drop the `basic-fungible-faucet` package and inject
    // a fully-populated component built directly from that metadata, rather than synthesizing the
    // schema-driven init entries.
    let mut packages = packages;
    let injected_fungible_faucet = if let Some(metadata) = faucet_metadata.as_ref() {
        if drop_basic_fungible_faucet_packages(&mut packages) {
            debug!("Building FungibleFaucet component from fungible-faucet-metadata block");
            Some(build_fungible_faucet_component(metadata)?)
        } else {
            None
        }
    } else {
        None
    };

    let mut init_seed = [0u8; 32];
    client.rng().fill_bytes(&mut init_seed);

    let mut builder = AccountBuilder::new(init_seed).account_type(account_type);

    // Only add the default auth component when no package provides one.
    let (auth_components, mut regular_components): (Vec<_>, Vec<_>) =
        process_packages(packages, &init_storage_data)?
            .into_iter()
            .partition(AccountComponent::is_auth_component);

    // Inject the directly-built fungible faucet component (if any) so the rest of the flow (policy
    // manager injection, schema commitment build) treats it like any other regular component.
    if let Some(component) = injected_fungible_faucet {
        regular_components.push(component);
    }

    // Faucet accounts require a token policy manager component. The CLI's standard
    // `basic-fungible-faucet` package only provides the faucet component itself, so add the default
    // `allow_all` policy manager implicitly.
    if should_add_implicit_token_policy_manager(&regular_components) {
        debug!("Adding implicit TokenPolicyManager component for fungible faucet");
        let policy_manager = TokenPolicyManager::builder()
            .active_mint_policy(MintPolicy::allow_all())
            .active_burn_policy(BurnPolicy::allow_all())
            .build();
        regular_components.extend(policy_manager);
    }
    if external_key.is_some() && !auth_components.is_empty() {
        return Err(CliError::InvalidArgument(
            "the given packages contribute an auth component, which cannot be combined with \
            --ecdsa-public-key"
                .to_string(),
        ));
    }

    // Add the auth component: one committing to the external ECDSA key, one from the packages, or a
    // generated default Falcon key.
    let uses_external_key = external_key.is_some();
    let key_pair = if let Some(public_key) = external_key {
        debug!("Adding ECDSA auth component for the external public key");
        builder = builder.with_component(AuthSingleSig::ecdsa_k256_keccak(public_key));
        None
    } else if auth_components.is_empty() {
        debug!("Adding default Falcon auth component");
        let kp = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
        builder = builder.with_component(AuthSingleSig::new(Approver::new(
            kp.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )));
        Some(kp)
    } else {
        debug!("Adding auth component from package");
        for component in auth_components {
            builder = builder.with_component(component);
        }
        None
    };

    // Add all regular (non-auth) components
    for component in regular_components {
        builder = builder.with_component(component);
    }

    let account = builder
        .build_with_schema_commitment()
        .map_err(|err| CliError::Account(err, "failed to build account".into()))?;

    // Only add the key to the keystore if we generated a default key type (Falcon)
    if let Some(key_pair) = key_pair {
        // Use the Keystore trait method which handles both key storage and account association
        keystore.add_key(&key_pair, account.id()).await.map_err(CliError::KeyStore)?;
        println!("Generated and stored Falcon512 authentication key in keystore.");
    } else if uses_external_key {
        println!(
            "Using external ECDSA public key for authentication (no key was generated or \
            stored; transactions must be signed by the external key holder)."
        );
    } else {
        println!("Using custom authentication component from package (no key generated).");
    }

    let _ = offline;

    #[cfg(feature = "testing")]
    if offline {
        client.prepare_offline_bootstrap().await?;
        println!("Offline mode enabled for local account creation.");
    }

    client.add_account(&account, false).await?;

    Ok(account)
}

/// Builds one [`AccountComponent`] from each package, prompting on stdin for the storage values
/// that the init data does not provide and the schema has no default for.
fn process_packages(
    packages: Vec<Package>,
    init_storage_data: &InitStorageData,
) -> Result<Vec<AccountComponent>, CliError> {
    let mut account_components = Vec::with_capacity(packages.len());

    for package in packages {
        if package.kind != TargetType::AccountComponent {
            return Err(CliError::InvalidArgument(format!(
                "package {} was built as a `{}`, not as an account component",
                package.name, package.kind
            )));
        }

        let component_metadata = AccountComponentMetadata::try_from(&package).map_err(|err| {
            CliError::Account(
                err,
                format!("failed to read account component metadata from package {}", package.name),
            )
        })?;

        // Entries for slots that this package does not define are ignored when the storage slots
        // are built, so the whole init data is passed and only the missing values are prompted.
        let mut init_data = init_storage_data.clone();
        for (value_name, requirement) in component_metadata.schema_requirements() {
            // A composite slot can be given as one slot-level value instead of one value per field.
            // The schema applies `default_value` itself when no entry is present.
            if init_data.value_entry(&value_name).is_some()
                || init_data.slot_value_entry(value_name.slot_name()).is_some()
                || requirement.default_value.is_some()
            {
                continue;
            }

            let description = requirement.description.unwrap_or("[No description]".into());
            println!(
                "Enter value for '{value_name}' - {description} (type: {}): ",
                requirement.r#type
            );
            std::io::stdout().flush()?;

            let mut input_value = String::new();
            std::io::stdin().read_line(&mut input_value)?;
            init_data.insert_value(value_name, input_value.trim()).map_err(|e| {
                CliError::AccountComponentError(
                    Box::new(e),
                    format!("error adding init storage value for Package {}", package.name),
                )
            })?;
        }

        let package_name = package.name.clone();
        let account_component =
            AccountComponent::from_package(package, &init_data).map_err(|e| {
                CliError::Account(
                    e,
                    format!("error instantiating component from Package {package_name}"),
                )
            })?;

        // Only exports marked with `@account_procedure` or `@auth_script` become account
        // procedures. A package with unmarked exports produces a component without procedures.
        if account_component.procedures().next().is_none() {
            eprintln!(
                "Warning: package {package_name} has no procedures marked with `@account_procedure` \
                or `@auth_script`."
            );
        }

        account_components.push(account_component);
    }

    Ok(account_components)
}

#[cfg(test)]
mod tests {
    use miden_client::account::StorageSlotName;
    use miden_client::account::component::{
        BasicWallet,
        FeltSchema,
        SchemaType,
        StorageSchema,
        StorageSlotSchema,
        TokenName,
        ValueSlotSchema,
        WordSchema,
    };
    use miden_client::assembly::CodeBuilder;
    use miden_client::asset::{AssetAmount, TokenSymbol};
    use miden_client::utils::{Serializable, hex_to_bytes};
    use miden_client::vm::{Section, SectionId};
    use miden_client::{Felt, Word};

    use super::*;

    const TEST_SLOT: &str = "miden::testing::marked_procs::slot";

    /// Assembles `code` into an account component package with an empty storage schema.
    fn test_component_package(code: &str) -> Package {
        test_component_package_with_schema(code, StorageSchema::default())
    }

    /// Assembles `code` into an account component package with the given storage schema.
    fn test_component_package_with_schema(code: &str, schema: StorageSchema) -> Package {
        let mut package = CodeBuilder::default()
            .compile_component_code("miden::testing::marked_procs", code)
            .expect("component code should compile")
            .into_package();
        let metadata = AccountComponentMetadata::new("marked-procs").with_storage_schema(schema);
        package.kind = TargetType::AccountComponent;
        package.sections =
            vec![Section::new(SectionId::ACCOUNT_COMPONENT_METADATA, metadata.to_bytes())];
        package
    }

    #[test]
    fn process_packages_rejects_non_component_package_kind() {
        let mut package = test_component_package("@account_procedure pub proc marked nop end");
        package.kind = TargetType::Library;

        let err = process_packages(vec![package], &InitStorageData::default())
            .expect_err("a library package should be rejected");

        assert!(
            err.to_string().contains("not as an account component"),
            "unexpected error: {err}"
        );
    }

    /// Builds a schema with one composite value slot whose four felts are named `a` to `d`.
    fn composite_slot_schema(default: Option<Felt>) -> StorageSchema {
        let felt = |name: &str| match default {
            Some(value) => {
                FeltSchema::new_typed_with_default(SchemaType::native_felt(), name, value)
            },
            None => FeltSchema::new_typed(SchemaType::native_felt(), name),
        };
        let word = WordSchema::new_value([felt("a"), felt("b"), felt("c"), felt("d")]);
        StorageSchema::new([(
            StorageSlotName::new(TEST_SLOT).unwrap(),
            StorageSlotSchema::Value(ValueSlotSchema::new(None, word)),
        )])
        .unwrap()
    }

    #[test]
    fn process_packages_accepts_slot_level_value_for_composite_slot() {
        let package = test_component_package_with_schema(
            "@account_procedure pub proc marked nop end",
            composite_slot_schema(None),
        );
        let mut init_data = InitStorageData::default();
        init_data.insert_value(TEST_SLOT, "0x1").unwrap();
        let expected = AccountComponentMetadata::try_from(&package)
            .unwrap()
            .storage_schema()
            .build_storage_slots(&init_data)
            .unwrap();

        // Without the slot-level check every field would be prompted on stdin, which is empty under
        // the test runner, and the empty values would conflict with the slot-level value.
        let components = process_packages(vec![package], &init_data)
            .expect("a slot-level value should satisfy every field of the slot");

        assert_eq!(components[0].storage_slots(), expected.as_slice());
    }

    #[test]
    fn process_packages_applies_schema_defaults_without_prompting() {
        let package = test_component_package_with_schema(
            "@account_procedure pub proc marked nop end",
            composite_slot_schema(Some(Felt::from(7u32))),
        );

        let components = process_packages(vec![package], &InitStorageData::default())
            .expect("defaults should satisfy every field of the slot");

        assert_eq!(components[0].storage_slots()[0].value(), Word::from([7u32, 7, 7, 7]));
    }

    #[test]
    fn process_packages_rejects_package_without_metadata() {
        let mut package = test_component_package("@account_procedure pub proc marked nop end");
        package.sections.clear();

        let err = process_packages(vec![package], &InitStorageData::default())
            .expect_err("a package without metadata should be rejected");

        assert!(
            err.to_string().contains("failed to read account component metadata"),
            "unexpected error: {err}"
        );
    }

    fn test_fungible_faucet_component() -> AccountComponent {
        FungibleFaucet::builder()
            .name(TokenName::new("TST").unwrap())
            .symbol(TokenSymbol::new("TST").unwrap())
            .decimals(8)
            .max_supply(AssetAmount::new(1_000_000).unwrap())
            .build()
            .unwrap()
            .into()
    }

    #[test]
    fn implicit_token_policy_manager_is_added_for_basic_faucet_accounts() {
        let regular_components = vec![test_fungible_faucet_component()];

        assert!(should_add_implicit_token_policy_manager(&regular_components));
    }

    #[test]
    fn implicit_token_policy_manager_is_skipped_when_component_already_present() {
        let mut regular_components: Vec<AccountComponent> = vec![test_fungible_faucet_component()];
        let policy_manager = TokenPolicyManager::builder()
            .active_mint_policy(MintPolicy::allow_all())
            .active_burn_policy(BurnPolicy::allow_all())
            .build();
        regular_components.extend(policy_manager);

        assert!(!should_add_implicit_token_policy_manager(&regular_components));
    }

    #[test]
    fn implicit_token_policy_manager_is_not_added_for_non_faucet_accounts() {
        let regular_components = vec![AccountComponent::from(BasicWallet)];

        assert!(!should_add_implicit_token_policy_manager(&regular_components));
    }

    // ECDSA PUBLIC KEY PARSING
    // --------------------------------------------------------------------------------------------

    /// The secp256k1 generator point (even y coordinate) in both SEC1 encodings.
    const GEN_COMPRESSED: &str =
        "0x0279be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798";
    const GEN_UNCOMPRESSED: &str = "0x0479be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b1\
        6f81798483ada7726a3c4655da4fbfc0e1108a8fd17b448a68554199c47d08ffb10d4b8";

    /// The point 6·G (odd y coordinate), so the odd-parity branch of the uncompressed encoding is
    /// exercised as well.
    const SIX_GEN_COMPRESSED: &str =
        "0x03fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057a1460297556";
    const SIX_GEN_UNCOMPRESSED: &str = "0x04fff97bd5755eeea420453a14355235d382f6472f8568a18b2f057\
        a1460297556ae12777aacfbb620f3be96017f45c560de80f0f6518fe4a03c870c36b075f297";

    #[test]
    fn parse_ecdsa_public_key_accepts_compressed_key() {
        let key = parse_ecdsa_public_key(GEN_COMPRESSED).expect("compressed key should parse");

        let expected = hex_to_bytes::<33>(GEN_COMPRESSED).unwrap();
        assert_eq!(key.to_bytes(), expected);
    }

    #[test]
    fn parse_ecdsa_public_key_accepts_uncompressed_key_with_even_y() {
        let from_uncompressed =
            parse_ecdsa_public_key(GEN_UNCOMPRESSED).expect("uncompressed key should parse");
        let from_compressed = parse_ecdsa_public_key(GEN_COMPRESSED).unwrap();

        assert_eq!(from_uncompressed, from_compressed);
    }

    #[test]
    fn parse_ecdsa_public_key_accepts_uncompressed_key_with_odd_y() {
        let from_uncompressed =
            parse_ecdsa_public_key(SIX_GEN_UNCOMPRESSED).expect("uncompressed key should parse");
        let from_compressed = parse_ecdsa_public_key(SIX_GEN_COMPRESSED).unwrap();

        assert_eq!(from_uncompressed, from_compressed);
    }

    #[test]
    fn parse_ecdsa_public_key_rejects_missing_hex_prefix() {
        let err = parse_ecdsa_public_key(&GEN_COMPRESSED[2..])
            .expect_err("a key without the 0x prefix should be rejected");

        assert!(err.to_string().contains("0x"), "unexpected error: {err}");
    }

    #[test]
    fn parse_ecdsa_public_key_rejects_invalid_length() {
        let err = parse_ecdsa_public_key("0x1234")
            .expect_err("a key with an unsupported length should be rejected");

        assert!(err.to_string().contains("length"), "unexpected error: {err}");
    }

    #[test]
    fn parse_ecdsa_public_key_rejects_compressed_x_not_on_curve() {
        // x = 5 has no square root of x³ + 7 on secp256k1, so no point has this x coordinate.
        let not_on_curve = "0x020000000000000000000000000000000000000000000000000000000000000005";

        parse_ecdsa_public_key(not_on_curve)
            .expect_err("a compressed key with no matching curve point should be rejected");
    }

    #[test]
    fn parse_ecdsa_public_key_rejects_uncompressed_point_not_on_curve() {
        // (1, 1) does not satisfy the curve equation.
        let not_on_curve =
            format!("0x04{}{}", format_args!("{:064x}", 1), format_args!("{:064x}", 1));

        parse_ecdsa_public_key(&not_on_curve)
            .expect_err("an uncompressed point off the curve should be rejected");
    }
}

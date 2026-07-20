use std::collections::BTreeMap;
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
    StorageSlotSchema,
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
use miden_client::keystore::Keystore;
use miden_client::transaction::TransactionRequestBuilder;
use miden_client::utils::Deserializable;
use miden_client::vm::{Package, SectionId};
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
/// A wallet account exposes functionality to sign transactions and
/// manage asset transfers. Additionally, more component templates can be added by specifying
/// a list of component template files.
#[derive(Debug, Parser, Clone)]
pub struct NewWalletCmd {
    /// Account type (`private` or `public`).
    #[arg(value_enum, short = 't', long = "account-type", default_value_t = CliAccountType::Private)]
    pub account_type: CliAccountType,
    /// Optional list of paths specifying additional components in the form of
    /// packages to add to the account.
    #[arg(short, long)]
    pub extra_packages: Vec<PathBuf>,
    /// Optional file path to a TOML file containing a list of key/values used for initializing
    /// storage. Each of these keys should map to the templated storage values within the passed
    /// list of component templates. The user will be prompted to provide values for any keys not
    /// present in the init storage data file.
    #[arg(short, long)]
    pub init_storage_data_path: Option<PathBuf>,
    /// If set, the newly created wallet will be deployed to the network by submitting an
    /// authentication transaction.
    #[arg(long, default_value_t = false)]
    pub deploy: bool,
    /// Seed local-only state so the wallet can be created and used for execution without a node.
    /// Only available when built with the `testing` feature.
    #[cfg_attr(
        feature = "testing",
        arg(long, default_value_t = false, conflicts_with = "deploy")
    )]
    #[cfg_attr(not(feature = "testing"), arg(skip = false))]
    pub offline: bool,
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
            self.deploy,
            self.offline,
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
/// If a package with an authentication component is provided via `-p`, it will be used for
/// the account. Otherwise, a default `RpoFalcon512` authentication component will be added
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
    /// List of files specifying package files used to create account components for the
    /// account. If any package contributes a `FungibleFaucet` component, the resulting account
    /// is treated as a fungible faucet (and an implicit `TokenPolicyManager` is installed when
    /// not already provided).
    #[arg(short, long)]
    pub packages: Vec<PathBuf>,
    /// Optional file path to a TOML file containing a list of key/values used for initializing
    /// storage. Each of these keys should map to the templated storage values within the passed
    /// list of component templates. The user will be prompted to provide values for any keys not
    /// present in the init storage data file.
    #[arg(short, long)]
    pub init_storage_data_path: Option<PathBuf>,
    /// If set, the newly created account will be deployed to the network by submitting an
    /// authentication transaction.
    #[arg(long, default_value_t = false)]
    pub deploy: bool,
    /// Seed local-only state so the account can be created and used for execution without a node.
    /// Only available when built with the `testing` feature.
    #[cfg_attr(
        feature = "testing",
        arg(long, default_value_t = false, conflicts_with = "deploy")
    )]
    #[cfg_attr(not(feature = "testing"), arg(skip = false))]
    pub offline: bool,
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
            self.deploy,
            self.offline,
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
fn load_packages(
    cli_config: &CliConfig,
    package_paths: &[PathBuf],
) -> Result<Vec<Package>, CliError> {
    let mut packages = Vec::with_capacity(package_paths.len());

    let packages_dir = &cli_config.package_directory;
    for path in package_paths {
        // If a user passes in a file with the `.masp` file extension, then we
        // leave the path as is; since it probably is a full path (this is the
        // case with cargo-miden for instance).
        let path = match path.extension() {
            None => {
                let path = path.with_extension(MIDEN_PACKAGE_EXTENSION);
                Ok(packages_dir.join(path))
            },
            Some(extension) => {
                if extension == OsStr::new(MIDEN_PACKAGE_EXTENSION) {
                    Ok(path.clone())
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

        let package = Package::read_from_bytes(&bytes).map_err(|e| {
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
/// `FungibleFaucet` embeds the token metadata and requires every storage slot to be initialized
/// to deploy the `basic-fungible-faucet` package. Rather than encode the schema's field-level
/// layout here, the component is built directly from the high-level metadata via the typed
/// builder, which produces the same code and storage layout the package would have.
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

/// Removes any package whose component name matches the upstream `FungibleFaucet` from the
/// list, since we'll inject the equivalent component directly from the user-supplied
/// `[fungible-faucet-metadata]` instead of going through the package's prompt-driven init-data
/// path. (The package files are typically distributed as `basic-fungible-faucet.masp` but the
/// `Package.name` field stores the component's full canonical name from `FungibleFaucet::NAME`.)
fn drop_basic_fungible_faucet_packages(packages: &mut Vec<Package>) -> bool {
    let before = packages.len();
    packages.retain(|pkg| pkg.name != FungibleFaucet::NAME);
    packages.len() != before
}

/// Loads the initialization storage data from an optional TOML file.
/// If None is passed, an empty object is returned.
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

/// Separates account components into auth and regular components.
///
/// Returns a tuple of (`auth_component`, `regular_components`).
/// Returns an error if multiple auth components are found.
fn separate_auth_components(
    components: Vec<AccountComponent>,
) -> Result<(Option<AccountComponent>, Vec<AccountComponent>), CliError> {
    let mut auth_component: Option<AccountComponent> = None;
    let mut regular_components = Vec::new();

    for component in components {
        let auth_proc_count = component.procedures().filter(|(_, is_auth)| *is_auth).count();

        match auth_proc_count {
            0 => regular_components.push(component),
            1 => {
                if auth_component.is_some() {
                    return Err(CliError::InvalidArgument(
                        "Multiple auth components found in packages. Only one auth component is allowed per account.".to_string()
                    ));
                }
                auth_component = Some(component);
            },
            _ => {
                return Err(CliError::InvalidArgument(
                    "Component has multiple auth procedures. Only one auth procedure is allowed per component.".to_string()
                ));
            },
        }
    }

    Ok((auth_component, regular_components))
}

/// Returns `true` when the CLI should inject a default `TokenPolicyManager` for a fungible
/// faucet account built from package components.
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
/// If no auth component is detected in the packages, a Falcon-based auth component will be added.
async fn create_client_account<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    keystore: &CliKeyStore,
    account_type: AccountType,
    package_paths: &[PathBuf],
    init_storage_data_path: Option<PathBuf>,
    deploy: bool,
    offline: bool,
) -> Result<Account, CliError> {
    if package_paths.is_empty() {
        return Err(CliError::InvalidArgument(format!(
            "Account must contain at least one component. To provide one, pass a package with the -p flag, like so:
{} -p <package_name>
            ", client_binary_name().display())));
    }

    // Load the component templates and initialization storage data.

    let cli_config = CliConfig::load()?;
    debug!("Loading packages...");
    let packages = load_packages(&cli_config, package_paths)?;
    debug!("Loaded {} packages", packages.len());
    debug!("Loading initialization storage data...");
    let (init_storage_data, faucet_metadata) =
        load_init_storage_data(init_storage_data_path.as_ref())?;
    debug!("Loaded initialization storage data");

    // `FungibleFaucet` requires every storage slot to be initialized. When the user provides
    // a `[fungible-faucet-metadata]` TOML block, drop the `basic-fungible-faucet` package and
    // inject a fully-populated component built directly from that metadata, rather than
    // synthesizing the schema-driven init entries.
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

    // Process packages and separate auth components from regular components
    let account_components = process_packages(packages, &init_storage_data)?;
    let (auth_component, mut regular_components) = separate_auth_components(account_components)?;

    // Inject the directly-built fungible faucet component (if any) so the rest of the flow
    // (policy manager injection, schema commitment build) treats it like any other regular
    // component.
    if let Some(component) = injected_fungible_faucet {
        regular_components.push(component);
    }

    // Faucet accounts require a token policy manager component. The CLI's standard
    // `basic-fungible-faucet` package only provides the faucet component itself, so add the
    // default `allow_all` policy manager implicitly.
    if should_add_implicit_token_policy_manager(&regular_components) {
        debug!("Adding implicit TokenPolicyManager component for fungible faucet");
        let policy_manager = TokenPolicyManager::builder()
            .active_mint_policy(MintPolicy::allow_all())
            .active_burn_policy(BurnPolicy::allow_all())
            .build();
        regular_components.extend(policy_manager);
    }
    // Add the auth component (either from packages or default Falcon)
    let key_pair = if let Some(auth_component) = auth_component {
        debug!("Adding auth component from package");
        builder = builder.with_auth_component(auth_component);
        None
    } else {
        debug!("Adding default Falcon auth component");
        let kp = AuthSecretKey::new_falcon512_poseidon2_with_rng(client.rng());
        builder = builder.with_auth_component(AuthSingleSig::new(Approver::new(
            kp.public_key().to_commitment(),
            AuthSchemeId::Falcon512Poseidon2,
        )));
        Some(kp)
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

    if deploy {
        deploy_account(client, &account).await?;
    }

    Ok(account)
}

/// Submits a deploy transaction to the node for the specified account.
async fn deploy_account<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    account: &Account,
) -> Result<(), CliError> {
    // Build a minimal transaction request. The transaction execution will naturally increment
    // the account nonce from 0 to 1, which deploys the account on-chain.
    // We don't need to call auth procedures directly as that must be done in the epilogue.
    let tx_request = TransactionRequestBuilder::new().build().map_err(|err| {
        CliError::Transaction(err.into(), "Failed to build deploy transaction".to_string())
    })?;

    client.submit_new_transaction(account.id(), tx_request).await?;
    Ok(())
}

fn process_packages(
    packages: Vec<Package>,
    init_storage_data: &InitStorageData,
) -> Result<Vec<AccountComponent>, CliError> {
    let mut account_components = Vec::with_capacity(packages.len());

    for package in packages {
        let mut value_entries = init_storage_data.values().clone();
        let mut map_entries = BTreeMap::new();

        let Some(component_metadata_section) = package.sections.iter().find(|section| {
            section.id.as_str() == (SectionId::ACCOUNT_COMPONENT_METADATA).as_str()
        }) else {
            continue;
        };

        let component_metadata = AccountComponentMetadata::read_from_bytes(
            &component_metadata_section.data,
        )
        .map_err(|err| {
            CliError::AccountComponentError(
                Box::new(err),
                format!(
                    "Failed to deserialize Account Component Metadata from package {}",
                    package.name
                ),
            )
        })?;

        // Preserve any provided map entries for map slots.
        for (slot_name, schema) in component_metadata.storage_schema().iter() {
            if matches!(schema, StorageSlotSchema::Map(_))
                && let Some(entries) = init_storage_data.map_entries(slot_name)
            {
                map_entries.insert(slot_name.clone(), entries.clone());
            }
        }

        for (value_name, requirement) in component_metadata.schema_requirements() {
            if value_entries.contains_key(&value_name) {
                // The user provided it through the TOML file, so we can skip it
                continue;
            }

            if let Some(default_value) = &requirement.default_value {
                // Use the schema's default value without prompting the user
                value_entries.insert(value_name, default_value.clone().into());
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
            let input_value = input_value.trim();
            value_entries.insert(value_name, input_value.to_string().into());
        }

        let init_data = InitStorageData::new(value_entries, map_entries).map_err(|e| {
            CliError::AccountComponentError(
                Box::new(e),
                format!("error creating InitStorageData for Package {}", package.name),
            )
        })?;
        let account_component =
            AccountComponent::from_package(&package, &init_data).map_err(|e| {
                CliError::Account(
                    e,
                    format!("error instantiating component from Package {}", package.name),
                )
            })?;

        account_components.push(account_component);
    }

    Ok(account_components)
}

#[cfg(test)]
mod tests {
    use miden_client::account::component::{BasicWallet, TokenName};
    use miden_client::asset::{AssetAmount, TokenSymbol};

    use super::*;

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
}

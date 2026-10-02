use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use clap::{Args, Parser, ValueEnum};
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
use miden_client::auth::{AuthSchemeId, AuthSecretKey, AuthSingleSig};
use miden_client::crypto::ecdsa_k256_keccak;
use miden_client::keystore::Keystore;
use miden_client::utils::Deserializable;
use miden_client::vm::{Package, TargetType};
use rand::{CryptoRng, Rng};
use serde::Deserialize;
use tracing::debug;

use crate::commands::account::set_default_account_if_unset;
use crate::commands::keys::{ECDSA_SCHEME_NAME, FALCON_SCHEME_NAME, scheme_name};
use crate::config::CliConfig;
use crate::errors::CliError;
use crate::utils::parse_ecdsa_public_key;
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

/// Selects an authentication scheme and an optional external ECDSA public key.
#[derive(Args, Clone, Debug)]
struct AuthArgs {
    /// Use the ECDSA k256/Keccak authentication scheme.
    ///
    /// With a `PUBLIC_KEY`, the account uses the external key and stores no secret key. The key
    /// must use a `0x`-prefixed, compressed or uncompressed SEC1 encoding. Without a value, the
    /// command generates an ECDSA secret key and stores it in the keystore.
    #[allow(clippy::option_option)]
    #[arg(
        long = ECDSA_SCHEME_NAME,
        visible_alias = "ecdsa",
        value_name = "PUBLIC_KEY",
        num_args = 0..=1,
        conflicts_with = "falcon512_poseidon2"
    )]
    ecdsa_k256_keccak: Option<Option<String>>,

    /// Generate and store a Falcon512/Poseidon2 authentication key.
    #[arg(
        long = FALCON_SCHEME_NAME,
        visible_alias = "falcon",
        default_value_t = false,
        conflicts_with = "ecdsa_k256_keccak"
    )]
    falcon512_poseidon2: bool,
}

impl AuthArgs {
    fn choice(&self) -> Result<AuthChoice, CliError> {
        match (&self.ecdsa_k256_keccak, self.falcon512_poseidon2) {
            (Some(Some(key)), false) => parse_ecdsa_public_key(key).map(AuthChoice::ExternalEcdsa),
            (Some(None), false) => Ok(AuthChoice::Generate(AuthSchemeId::EcdsaK256Keccak)),
            (None, true) => Ok(AuthChoice::Generate(AuthSchemeId::Falcon512Poseidon2)),
            (None, false) => Ok(AuthChoice::Default),
            (Some(_), true) => Err(CliError::InvalidArgument(format!(
                "--{ECDSA_SCHEME_NAME} and --{FALCON_SCHEME_NAME} cannot be used together"
            ))),
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
    #[command(flatten)]
    auth: AuthArgs,
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
            self.auth.choice()?,
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
/// An authentication scheme can also be selected explicitly with `--ecdsa-k256-keccak [PUBLIC_KEY]`
/// or `--falcon512-poseidon2` (aliases: `--ecdsa`, `--falcon`). These flags are mutually exclusive
/// with each other and with auth component packages. When an ECDSA public key is given, the account
/// commits to that externally-held key and no secret key is stored; otherwise a key of the selected
/// scheme is generated and stored in the keystore.
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
    #[command(flatten)]
    auth: AuthArgs,
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
            self.auth.choice()?,
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

/// The user's authentication scheme selection for a new account, from the mutually exclusive
/// `--ecdsa-k256-keccak` and `--falcon512-poseidon2` flags.
enum AuthChoice {
    /// No scheme flag given: an auth component from the packages wins, otherwise a Falcon key is
    /// generated.
    Default,
    /// An explicitly selected scheme with a freshly generated, keystore-held secret key.
    Generate(AuthSchemeId),
    /// An externally-held ECDSA public key: the account commits to it and no key is stored.
    ExternalEcdsa(ecdsa_k256_keccak::PublicKey),
}

/// Describes how the account authentication component was created.
enum AuthOutcome {
    Generated(AuthSecretKey),
    External,
    Package,
}

/// Adds the authentication component selected by `auth_choice`.
///
/// The component uses an external public key, a generated key, or package data. A package takes
/// precedence when the user does not select a scheme. The function generates a Falcon key when no
/// package supplies authentication data. An explicit selection conflicts with package
/// authentication data.
///
/// Returns the updated builder and the authentication outcome.
fn add_auth_component<R: Rng + CryptoRng>(
    builder: AccountBuilder,
    auth_choice: AuthChoice,
    auth_components: Vec<AccountComponent>,
    rng: &mut R,
) -> Result<(AccountBuilder, AuthOutcome), CliError> {
    let scheme = match (auth_choice, auth_components.is_empty()) {
        (AuthChoice::Default, false) => {
            debug!("Adding auth component from package");
            let builder = auth_components.into_iter().fold(builder, AccountBuilder::with_component);
            return Ok((builder, AuthOutcome::Package));
        },
        (_, false) => {
            return Err(CliError::InvalidArgument(format!(
                "the given packages contribute an auth component, which cannot be combined \
                    with --{ECDSA_SCHEME_NAME} or --{FALCON_SCHEME_NAME}"
            )));
        },
        (AuthChoice::ExternalEcdsa(public_key), true) => {
            debug!("Adding ECDSA auth component for the external public key");
            let builder = builder.with_component(AuthSingleSig::ecdsa_k256_keccak(public_key));
            return Ok((builder, AuthOutcome::External));
        },
        (AuthChoice::Generate(scheme), true) => scheme,
        (AuthChoice::Default, true) => AuthSchemeId::Falcon512Poseidon2,
    };

    debug!("Adding auth component with a generated {scheme} key");
    let key = AuthSecretKey::with_scheme_and_rng(scheme, rng).map_err(|err| {
        CliError::InvalidArgument(format!("failed to generate a {scheme} key: {err}"))
    })?;
    let builder = builder.with_component(AuthSingleSig::from_public_key(key.public_key()));
    Ok((builder, AuthOutcome::Generated(key)))
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
/// The auth component follows `auth_choice`: an explicitly selected scheme either commits to the
/// given external ECDSA key (storing no secret key) or generates and stores a key of that scheme.
/// With no explicit selection, an auth component from the packages wins, and a Falcon key is
/// generated when the packages provide none.
async fn create_client_account<AUTH: Keystore + Sync + 'static>(
    client: &mut Client<AUTH>,
    keystore: &CliKeyStore,
    account_type: AccountType,
    package_paths: &[PathBuf],
    init_storage_data_path: Option<PathBuf>,
    offline: bool,
    auth_choice: AuthChoice,
) -> Result<Account, CliError> {
    if package_paths.is_empty() {
        return Err(CliError::InvalidArgument(
            "Account must contain at least one component".to_string(),
        ));
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

    let builder = AccountBuilder::new(init_seed).account_type(account_type);

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
    let (mut builder, auth_outcome) =
        add_auth_component(builder, auth_choice, auth_components, client.rng())?;

    // Add all regular (non-auth) components
    for component in regular_components {
        builder = builder.with_component(component);
    }

    let account = builder
        .build_with_schema_commitment()
        .map_err(|err| CliError::Account(err, "failed to build account".into()))?;

    match auth_outcome {
        AuthOutcome::Generated(key) => {
            keystore.add_key(&key, account.id()).await.map_err(CliError::KeyStore)?;
            println!(
                "Generated and stored {} authentication key in keystore.",
                scheme_name(key.auth_scheme())
            );
        },
        AuthOutcome::External => {
            println!(
                "Using external ECDSA public key for authentication (no key was generated or \
                stored; transactions must be signed by the external key holder)."
            );
        },
        AuthOutcome::Package => {
            println!("Using custom authentication component from package (no key generated).");
        },
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
    use miden_client::utils::Serializable;
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
}

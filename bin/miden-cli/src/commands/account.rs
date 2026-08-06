use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use clap::Parser;
use comfy_table::{Cell, ContentArrangement, presets};
use miden_client::account::component::{FungibleFaucet, MIDEN_PACKAGE_EXTENSION};
use miden_client::account::{
    Account,
    AccountCode,
    AccountId,
    AccountInterfaceExt,
    StorageSlotContent,
};
use miden_client::address::{Address, AddressInterface, NetworkId, RoutingParameters};
use miden_client::asset::Asset;
use miden_client::rpc::domain::account::GetAccountRequest;
use miden_client::rpc::{GrpcClient, NodeRpcClient, VerifyingRpcClient};
use miden_client::transaction::{AccountComponentInterface, AccountInterface};
use miden_client::utils::base_units_to_tokens;
use miden_client::vm::{Package, PackageExport};
use miden_client::{Client, PrettyPrint, Word, ZERO};

use crate::commands::new_account::load_packages;
use crate::config::{CliConfig, RpcConfig};
use crate::errors::CliError;
use crate::utils::{parse_account_id, split_procedure_target};
use crate::{client_binary_name, create_dynamic_table};

pub const DEFAULT_ACCOUNT_ID_KEY: &str = "default_account_id";

// ACCOUNT COMMAND
// ================================================================================================

/// View and manage accounts. Defaults to `list` command.
#[derive(Default, Debug, Clone, Parser)]
#[allow(clippy::option_option)]
pub struct AccountCmd {
    /// List all accounts monitored by this client (default action).
    #[arg(short, long, group = "action")]
    list: bool,
    /// Show details of the account for the specified ID or hex prefix.
    #[arg(short, long, group = "action", value_name = "ID")]
    show: Option<String>,
    /// List the procedures exposed by the account for the specified ID (or hex prefix), resolving
    /// each procedure's name and signature from `.masp` packages.
    ///
    /// Accepts either `<ID>` to list every procedure, or `<ID>:<PROCEDURE>` to resolve a single
    /// procedure by name.
    #[arg(long, group = "action", value_name = "ID[:PROCEDURE]")]
    inspect: Option<String>,
    /// Additional package files (`.masp`) used to resolve procedure MAST roots to names and
    /// signatures, on top of the packages in the configured packages directory.
    ///
    /// May be passed multiple times. On a duplicate MAST root, the passed packages take
    /// precedence.
    #[arg(short, long, value_name = "FILE", requires = "inspect")]
    package: Vec<PathBuf>,
    /// When using --inspect, also print the MASM disassembly of each procedure.
    #[arg(short, long, requires = "inspect")]
    verbose: bool,
    /// Manages default account for transaction execution.
    ///
    /// If no ID is provided it will display the current default account ID.
    /// If "none" is provided it will remove the default account else it will set the default
    /// account to the provided ID.
    #[arg(short, long, group = "action", value_name = "ID")]
    default: Option<Option<String>>,
}

impl AccountCmd {
    pub async fn execute<AUTH>(&self, mut client: Client<AUTH>) -> Result<(), CliError> {
        let cli_config = CliConfig::load()?;
        match self {
            AccountCmd {
                list: false,
                show: Some(id),
                default: None,
                ..
            } => {
                let account_id = parse_account_id(&client, id).await?;
                show_account(&client, account_id, &cli_config.rpc).await?;
            },
            AccountCmd {
                list: false,
                show: None,
                inspect: Some(target),
                default: None,
                ..
            } => {
                let (id, procedure) = split_procedure_target(target);
                let account_id = parse_account_id(&client, id).await?;

                // Explicit `--package` files take precedence over the configured packages
                // directory (on a duplicate MAST root the first package wins), but both are
                // consulted so default names still resolve alongside the passed packages.
                let mut packages = load_packages(&cli_config, &self.package)?;
                packages.extend(load_packages_from_directory(&cli_config.package_directory)?);

                inspect_account(
                    &client,
                    account_id,
                    &cli_config.rpc,
                    procedure,
                    &packages,
                    self.verbose,
                )
                .await?;
            },
            AccountCmd {
                list: false,
                show: None,
                default: Some(id),
                ..
            } => {
                match id {
                    None => {
                        let default_account: AccountId = client
                            .get_setting(DEFAULT_ACCOUNT_ID_KEY.to_string())
                            .await?
                            .ok_or(CliError::Config(
                                "Default account".to_string().into(),
                                "No default account found in the client's store".to_string(),
                            ))?;
                        println!("Current default account ID: {default_account}");
                    },
                    Some(id) if id == "none" => {
                        client.remove_setting(DEFAULT_ACCOUNT_ID_KEY.to_string()).await?;
                        println!("Removing default account...");
                    },
                    Some(id) => {
                        let account_id: AccountId = parse_account_id(&client, id).await?;

                        // Check whether we're tracking that account
                        let (account, _) = client.account_reader(account_id).header().await?;

                        client
                            .set_setting(DEFAULT_ACCOUNT_ID_KEY.to_string(), account.id())
                            .await?;
                        println!("Setting default account to {id}...");
                    },
                }
            },
            _ => {
                list_accounts(client).await?;
            },
        }
        Ok(())
    }
}

// LIST ACCOUNTS
// ================================================================================================

async fn list_accounts<AUTH>(client: Client<AUTH>) -> Result<(), CliError> {
    let accounts = client.get_account_headers().await?;

    let mut table = create_dynamic_table(&["Account ID", "Kind", "Type", "Nonce", "Status"]);
    for (acc, _acc_seed) in &accounts {
        let reader = client.account_reader(acc.id());
        let status = reader.status().await?.to_string();
        let token_symbol = get_faucet_component(&client, acc.id())
            .await
            .ok()
            .map(|faucet| faucet.symbol().to_string());

        table.add_row(vec![
            acc.id().to_hex(),
            account_kind_display_name(token_symbol.as_deref()),
            acc.id().account_type().to_string(),
            acc.nonce().as_canonical_u64().to_string(),
            status,
        ]);
    }

    println!("{table}");
    Ok(())
}

// SHOW ACCOUNT
// ================================================================================================

async fn show_account<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
    rpc_config: &RpcConfig,
) -> Result<(), CliError> {
    let account = load_account(client, account_id, rpc_config).await?;

    let network_id = rpc_config.endpoint.0.to_network_id();
    let token_symbol = faucet_component_from_account(&account)
        .ok()
        .map(|faucet| faucet.symbol().to_string());
    print_summary_table(&account, network_id, token_symbol.as_deref());

    // Vault Table
    {
        let assets = account.vault().assets();
        println!("Assets: ");

        let mut table = create_dynamic_table(&["Asset Type", "Faucet", "Amount"]);
        for asset in assets {
            let (asset_type, faucet, amount) = match asset {
                Asset::Fungible(fungible_asset) => {
                    let faucet_id = fungible_asset.faucet_id();
                    let asset_amount = fungible_asset.amount();
                    let (faucet, amount) = match get_faucet_component(client, faucet_id).await {
                        Ok(faucet_component) => (
                            faucet_component.symbol().to_string(),
                            base_units_to_tokens(asset_amount, faucet_component.decimals()),
                        ),
                        Err(_) => (faucet_id.prefix().to_hex(), asset_amount.as_u64().to_string()),
                    };
                    ("Fungible Asset", faucet, amount)
                },
                Asset::NonFungible(non_fungible_asset) => {
                    // TODO: Display non-fungible assets more clearly.
                    (
                        "Non Fungible Asset",
                        non_fungible_asset.faucet_id().prefix().to_hex(),
                        1.0.to_string(),
                    )
                },
            };
            table.add_row(vec![asset_type, &faucet, &amount.clone()]);
        }

        println!("{table}\n");
    }

    // Storage Table
    {
        let account_storage = account.storage();

        println!("Storage: \n");

        let mut table = create_dynamic_table(&["Slot Name", "Slot Type", "Value/Commitment"]);

        for entry in account_storage.slots() {
            let item = account_storage.get_item(entry.name()).map_err(|err| {
                CliError::Account(err, format!("failed to fetch slot {}", entry.name()))
            })?;

            // Last entry is reserved so I don't think the user cares about it. Also, to keep the
            // output smaller, if the [StorageSlot] is a value and it's 0 we assume it's not
            // initialized and skip it
            if matches!(entry.content(), StorageSlotContent::Value(_)) && item == [ZERO; 4].into() {
                continue;
            }

            let slot_type = match entry.content() {
                StorageSlotContent::Value(_) => "Value",
                StorageSlotContent::Map(_) => "Map",
            };
            table.add_row(vec![entry.name().as_str(), slot_type, &item.to_hex()]);
        }
        println!("{table}\n");
    }

    Ok(())
}

// INSPECT ACCOUNT
// ================================================================================================

/// Placeholder shown when a procedure's name or signature could not be resolved from any package.
const NO_VALUE: &str = "<unresolved>";

/// A single account procedure with its name, signature, and originating package, if known.
///
/// The optional fields are only populated for procedures whose MAST root was matched against a
/// package export; for the rest only the MAST root is known.
#[derive(Clone)]
struct ProcedureMetadata {
    mast_root: Word,
    name: Option<String>,
    signature: Option<String>,
    package: Option<String>,
}

/// Lists the account's procedures, resolving each name and signature from the given packages.
///
/// The full listing is grouped: procedures whose name resolved are shown in a table (with the
/// package they came from), and the rest are listed by their MAST root under a hint to pass
/// `--package`. With `procedure_filter`, only that procedure is printed (an error if it cannot be
/// resolved). With `verbose`, each procedure's MASM disassembly follows.
async fn inspect_account<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
    rpc_config: &RpcConfig,
    procedure_filter: Option<&str>,
    packages: &[Package],
    verbose: bool,
) -> Result<(), CliError> {
    let code = resolve_account_code(client, account_id, rpc_config).await?;

    let exports = collect_package_procedure_exports(packages);
    let procedures: Vec<ProcedureMetadata> = code
        .procedure_roots()
        .map(|mast_root| {
            exports.get(&mast_root).cloned().unwrap_or(ProcedureMetadata {
                mast_root,
                name: None,
                signature: None,
                package: None,
            })
        })
        .collect();

    // Single-procedure lookup: print just the requested procedure, or error if it cannot be
    // resolved. A name can only be matched once a package supplies it, so a miss may mean the
    // account does not expose it *or* that its defining package was not provided.
    if let Some(procedure) = procedure_filter {
        let matches: Vec<&ProcedureMetadata> = procedures
            .iter()
            .filter(|proc| proc.name.as_deref() == Some(procedure))
            .collect();
        if matches.is_empty() {
            return Err(CliError::Input(format!(
                "no procedure named `{procedure}` could be resolved for account {account_id}; it \
                 may not be exposed by the account, or its defining package was not provided (pass \
                 it with --package)",
            )));
        }
        print_procedure_table(&matches);
        if verbose {
            print_disassembly(&matches, &code);
        }
        return Ok(());
    }

    // Full listing: split resolved from unresolved, each keeping the account's procedure order.
    let (resolved, unresolved): (Vec<&ProcedureMetadata>, Vec<&ProcedureMetadata>) =
        procedures.iter().partition(|proc| proc.name.is_some());

    println!(
        "Account {account_id} — {} procedures ({} resolved, {} unresolved)",
        procedures.len(),
        resolved.len(),
        unresolved.len(),
    );

    if !resolved.is_empty() {
        println!("\nResolved ({}):", resolved.len());
        print_procedure_table(&resolved);
    }

    if !unresolved.is_empty() {
        println!(
            "\nUnresolved ({}) — pass --package <FILE.masp> to resolve names:",
            unresolved.len()
        );
        for proc in &unresolved {
            println!("  {}", proc.mast_root.to_hex());
        }
    }

    if verbose {
        let all: Vec<&ProcedureMetadata> = procedures.iter().collect();
        print_disassembly(&all, &code);
    }

    Ok(())
}

/// Prints a table of procedures with their resolved name, originating package, signature, and full
/// MAST root.
fn print_procedure_table(procedures: &[&ProcedureMetadata]) {
    let mut table = create_dynamic_table(&["Procedure", "Package", "Signature", "MAST Root"]);
    for proc in procedures {
        table.add_row(vec![
            proc.name.as_deref().unwrap_or(NO_VALUE).to_string(),
            proc.package.as_deref().unwrap_or(NO_VALUE).to_string(),
            proc.signature.as_deref().unwrap_or(NO_VALUE).to_string(),
            proc.mast_root.to_hex(),
        ]);
    }
    println!("{table}");
}

/// Prints the MASM disassembly of the given procedures, in the account's procedure order.
///
/// Only the requested procedures are disassembled, so a single-procedure lookup does not pay to
/// render the whole account.
fn print_disassembly(procedures: &[&ProcedureMetadata], code: &AccountCode) {
    let names: HashMap<Word, &str> = procedures
        .iter()
        .map(|proc| (proc.mast_root, proc.name.as_deref().unwrap_or(NO_VALUE)))
        .collect();

    for (mast_root, printable) in code.procedure_roots().zip(code.printable_procedures()) {
        if let Some(name) = names.get(&mast_root) {
            println!("\nProcedure {name} ({}):", mast_root.to_hex());
            println!("{}", printable.to_pretty_string());
        }
    }
}

/// Builds a lookup from procedure MAST root to the procedure exported under it across the given
/// packages.
///
/// When the same MAST root is exported by more than one package the first is kept and a warning is
/// emitted, so the resolved metadata is never silently taken from an ambiguous source.
fn collect_package_procedure_exports(packages: &[Package]) -> HashMap<Word, ProcedureMetadata> {
    let mut exports: HashMap<Word, ProcedureMetadata> = HashMap::new();
    for package in packages {
        let package_name = package.name.to_string();
        for export in package.manifest.exports() {
            if let PackageExport::Procedure(procedure) = export {
                match exports.entry(procedure.digest) {
                    Entry::Occupied(existing) => {
                        // Entries in the map always carry the package they were resolved from.
                        let first = existing.get().package.as_deref().unwrap_or_default();
                        if first != package_name {
                            eprintln!(
                                "Warning: procedure {} is exported by multiple packages ({first}, \
                                 {package_name}); resolving it from {first}.",
                                procedure.digest.to_hex(),
                            );
                        }
                    },
                    Entry::Vacant(slot) => {
                        slot.insert(ProcedureMetadata {
                            mast_root: procedure.digest,
                            name: Some(export.name().to_string()),
                            signature: procedure.signature.as_ref().map(ToString::to_string),
                            package: Some(package_name.clone()),
                        });
                    },
                }
            }
        }
    }
    exports
}

/// Reads every `.masp` package found recursively under `dir`. A missing directory yields no
/// packages rather than an error, so inspection still falls back to bare MAST roots.
fn load_packages_from_directory(dir: &Path) -> Result<Vec<Package>, CliError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut package_paths = Vec::new();
    collect_masp_files(dir, &mut package_paths)?;

    // Paths already carry the `.masp` extension, so `load_packages` uses them as-is and never
    // consults the config's packages directory.
    load_packages(&CliConfig::default(), &package_paths)
}

/// Recursively collects the paths of all `.masp` files under `dir` into `paths`.
fn collect_masp_files(dir: &Path, paths: &mut Vec<PathBuf>) -> Result<(), CliError> {
    let entries = std::fs::read_dir(dir).map_err(|err| {
        CliError::Config(
            Box::new(err),
            format!("failed to read packages directory {}", dir.display()),
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|err| {
            CliError::Config(
                Box::new(err),
                format!("failed to read entry in packages directory {}", dir.display()),
            )
        })?;
        let path = entry.path();
        if path.is_dir() {
            collect_masp_files(&path, paths)?;
        } else if path.extension().and_then(|ext| ext.to_str()) == Some(MIDEN_PACKAGE_EXTENSION) {
            paths.push(path);
        }
    }

    Ok(())
}

// HELPERS
// ================================================================================================

/// Resolves the code of `account_id`, falling back to the network when the client does not track
/// the account locally. The default request carries no vault or storage, so only the code is
/// fetched.
async fn resolve_account_code<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
    rpc_config: &RpcConfig,
) -> Result<AccountCode, CliError> {
    if let Some(code) = client.get_account_code(account_id).await? {
        return Ok(code);
    }

    println!("Account {account_id} is not tracked by the client. Fetching from the network...");

    let rpc_client = VerifyingRpcClient::new(GrpcClient::new(
        &rpc_config.endpoint.clone().into(),
        rpc_config.timeout_ms,
    ));

    let (_, account_proof) = rpc_client
        .get_account(account_id, GetAccountRequest::new())
        .await
        .map_err(|err| {
            CliError::Input(format!("Unable to fetch account {account_id} from the network: {err}"))
        })?;

    account_proof.account_code().cloned().ok_or(CliError::Input(format!(
        "Account {account_id} is private and not tracked by the client",
    )))
}

/// Loads the account for `account_id`, falling back to fetching it from the network when the client
/// does not track it locally.
async fn load_account<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
    rpc_config: &RpcConfig,
) -> Result<Account, CliError> {
    if let Some(account) = client.get_account(account_id).await? {
        return Ok(account);
    }

    println!("Account {account_id} is not tracked by the client. Fetching from the network...");

    let rpc_client = VerifyingRpcClient::new(GrpcClient::new(
        &rpc_config.endpoint.clone().into(),
        rpc_config.timeout_ms,
    ));

    let fetched_account = rpc_client.get_account_details(account_id).await.map_err(|err| {
        CliError::Input(format!("Unable to fetch account {account_id} from the network: {err}"))
    })?;

    fetched_account.ok_or(CliError::Input(format!(
        "Account {account_id} is private and not tracked by the client",
    )))
}

/// Prints a summary table with account information.
fn print_summary_table(account: &Account, network_id: NetworkId, token_symbol: Option<&str>) {
    let mut table = create_dynamic_table(&["Account Information"]);
    table
        .load_preset(presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth);

    table.add_row(vec![Cell::new("Address"), Cell::new(account_bech_32(account, network_id))]);
    table.add_row(vec![Cell::new("Account ID (hex)"), Cell::new(account.id().to_string())]);
    table.add_row(vec![
        Cell::new("Account Commitment"),
        Cell::new(account.to_commitment().to_string()),
    ]);
    table.add_row(vec![Cell::new("Kind"), Cell::new(account_kind_display_name(token_symbol))]);
    table.add_row(vec![Cell::new("Type"), Cell::new(account.id().account_type().to_string())]);
    table.add_row(vec![
        Cell::new("Code Commitment"),
        Cell::new(account.code().commitment().to_string()),
    ]);
    table.add_row(vec![Cell::new("Vault Root"), Cell::new(account.vault().root().to_string())]);
    table.add_row(vec![
        Cell::new("Storage Root"),
        Cell::new(account.storage().to_commitment().to_string()),
    ]);
    table.add_row(vec![
        Cell::new("Nonce"),
        Cell::new(account.nonce().as_canonical_u64().to_string()),
    ]);

    println!("{table}\n");
}

/// Loads the tracked account for `account_id` and reconstructs its [`FungibleFaucet`] component.
///
/// # Errors
/// Returns an error if the account is not tracked by the client or its faucet metadata can't be
/// read.
async fn get_faucet_component<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
) -> Result<FungibleFaucet, CliError> {
    let account = client.get_account(account_id).await?.ok_or_else(|| {
        CliError::Input(format!("account {account_id} not tracked by the client"))
    })?;

    faucet_component_from_account(&account)
}

/// Reconstructs the [`FungibleFaucet`] component from a materialized [`Account`].
///
/// # Errors
/// Returns an error if the account's faucet metadata can't be read.
fn faucet_component_from_account(account: &Account) -> Result<FungibleFaucet, CliError> {
    let account_id = account.id();
    FungibleFaucet::try_from(account).map_err(|err| {
        CliError::Faucet(err.into(), format!("Failed to read faucet metadata for {account_id}"))
    })
}

/// Returns the account's kind for display. If `token_symbol` is provided the account is rendered as
/// a fungible faucet (the symbol is appended); otherwise it's labelled "Regular".
///
/// The on-chain `AccountType` only encodes account visibility (`public` / `private`), so
/// faucet-vs-wallet has to be inferred by the caller from the attached components.
fn account_kind_display_name(token_symbol: Option<&str>) -> String {
    if let Some(symbol) = token_symbol {
        format!("Fungible faucet (token symbol: {symbol})")
    } else {
        "Regular".to_string()
    }
}

/// Returns `true` if the account code exposes the [`BasicWallet`] component interface.
///
/// Takes the [`AccountCode`] rather than the full [`Account`] so callers can avoid loading the
/// account's vault and storage just to inspect its interface.
pub(crate) fn account_code_has_basic_wallet(account_id: AccountId, code: &AccountCode) -> bool {
    AccountInterface::from_code(account_id, code)
        .components()
        .iter()
        .any(|c| matches!(c, AccountComponentInterface::BasicWallet))
}

/// Sets the provided account ID as the default account in the client's store, if not set already.
pub(crate) async fn set_default_account_if_unset<AUTH>(
    client: &mut Client<AUTH>,
    account_id: AccountId,
) -> Result<(), CliError> {
    if client
        .get_setting::<AccountId>(DEFAULT_ACCOUNT_ID_KEY.to_string())
        .await?
        .is_some()
    {
        return Ok(());
    }

    client.set_setting(DEFAULT_ACCOUNT_ID_KEY.to_string(), account_id).await?;

    println!("Setting account {account_id} as the default account ID.");
    println!(
        "You can unset it with `{} account --default none`.",
        client_binary_name().display()
    );

    Ok(())
}

fn account_bech_32(account: &Account, network_id: NetworkId) -> String {
    let account_id = account.id();
    let account_interface = AccountInterface::from_account(account);

    let mut address = Address::new(account_id);
    if account_interface
        .components()
        .iter()
        .any(|c| matches!(c, AccountComponentInterface::BasicWallet))
    {
        address =
            address.with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));
    }

    address.encode(network_id)
}

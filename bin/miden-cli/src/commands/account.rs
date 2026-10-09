use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::path::{Path, PathBuf};

use clap::Parser;
use comfy_table::{Cell, ContentArrangement, presets};
use miden_client::account::component::{
    FungibleFaucet,
    MIDEN_PACKAGE_EXTENSION,
    NonFungibleFaucet,
};
use miden_client::account::{
    AccountCode,
    AccountId,
    AccountInterfaceExt,
    PartialAccount,
    PartialStorage,
    StorageSlotType,
};
use miden_client::address::{Address, AddressInterface, NetworkId, RoutingParameters};
use miden_client::asset::{AssetVault, PartialVault, TokenSymbol};
use miden_client::rpc::domain::account::{GetAccountRequest, VaultFetch};
use miden_client::rpc::{GrpcClient, NodeRpcClient, VerifyingRpcClient};
use miden_client::transaction::{AccountComponentInterface, AccountInterface};
use miden_client::vm::{Package, PackageExport};
use miden_client::{Client, PrettyPrint, Word, ZERO};

use crate::commands::new_account::load_packages;
use crate::config::{CliConfig, RpcConfig};
use crate::errors::CliError;
use crate::utils::{base_units_to_tokens, parse_account_id, split_procedure_target};
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
    /// May be passed multiple times. On a duplicate MAST root, the passed packages take precedence.
    #[arg(short, long, value_name = "FILE", requires = "inspect")]
    package: Vec<PathBuf>,
    /// When using --inspect, also print the MASM disassembly of each procedure.
    #[arg(short, long, requires = "inspect")]
    verbose: bool,
    /// Manages default account for transaction execution.
    ///
    /// If no ID is provided it will display the current default account ID. If "none" is provided
    /// it will remove the default account else it will set the default account to the provided ID.
    #[arg(short, long, group = "action", value_name = "ID")]
    default: Option<Option<String>>,
    /// Registers the account with the specified ID (or hex prefix) on the network allowlist.
    ///
    /// Only an account that this client tracks can be registered. When the network funds registered
    /// accounts, the node pays the account a public note with the native asset. The note can take a
    /// few blocks to commit. Run `sync` until the note arrives, then `consume-notes` to create the
    /// account on chain with it.
    #[arg(long, group = "action", value_name = "ID", requires = "invitation_code")]
    register: Option<String>,
    /// Invitation code that registers the account named by --register.
    #[arg(long, value_name = "CODE", requires = "register")]
    invitation_code: Option<String>,
}

impl AccountCmd {
    pub async fn execute<AUTH>(&self, client: Client<AUTH>) -> Result<(), CliError> {
        let cli_config = CliConfig::load()?;
        match self {
            AccountCmd {
                list: false,
                show: Some(id),
                default: None,
                ..
            } => {
                let account_id = parse_account_id(&client, id).await?;
                show_account(&client, account_id, &cli_config).await?;
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

                // Explicit `--package` files take precedence over the configured packages directory
                // (on a duplicate MAST root the first package wins), but both are consulted so
                // default names still resolve alongside the passed packages.
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
                default: None,
                register: Some(id),
                invitation_code: Some(invitation_code),
                ..
            } => {
                let account_id = parse_account_id(&client, id).await?;
                client.register_account(account_id, invitation_code).await?;

                println!("Registered account {} on the network allowlist.", account_id.to_hex());
                println!(
                    "To use the funding note, if the network sends one, run `{bin} sync` and then \
                     `{bin} consume-notes --account {id}`.",
                    bin = client_binary_name().display(),
                    id = account_id.to_hex()
                );
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
                        let removed =
                            client.remove_setting(DEFAULT_ACCOUNT_ID_KEY.to_string()).await?;

                        if removed {
                            println!("Default account removed");
                        } else {
                            println!("No default account was set");
                        }
                    },
                    Some(id) => {
                        let account_id: AccountId = parse_account_id(&client, id).await?;

                        // Check whether we're tracking that account
                        let (account, _) = client.account_reader(account_id).header().await?;

                        client
                            .set_setting(DEFAULT_ACCOUNT_ID_KEY.to_string(), account.id())
                            .await?;

                        println!("Default account set to {}", account.id());
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
        let kind = if let Ok((symbol, _)) = get_faucet_token_info(&client, acc.id()).await {
            AccountKind::FungibleFaucet(symbol)
        } else if let Ok(symbol) = get_non_fungible_faucet_symbol(&client, acc.id()).await {
            AccountKind::NonFungibleFaucet(symbol)
        } else {
            AccountKind::Regular
        };

        table.add_row(vec![
            acc.id().to_hex(),
            kind.to_string(),
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
    cli_config: &CliConfig,
) -> Result<(), CliError> {
    let account = load_partial_account(client, account_id, &cli_config.rpc).await?;

    let network_id = cli_config.network_id()?;
    let header_slot_value =
        |slot_name| Some(account.storage().header().find_slot_header_by_name(slot_name)?.value());
    let kind = if let Some((symbol, _)) = header_slot_value(FungibleFaucet::token_config_slot())
        .and_then(|value| decode_token_config(account_id, value).ok())
    {
        AccountKind::FungibleFaucet(symbol)
    } else if let Some(symbol) = header_slot_value(NonFungibleFaucet::symbol_slot())
        .and_then(|value| decode_non_fungible_symbol(account_id, value).ok())
    {
        AccountKind::NonFungibleFaucet(symbol)
    } else {
        AccountKind::Regular
    };
    print_summary_table(&account, network_id, &kind);

    // Vault Tables
    {
        let mut fungible_table = create_dynamic_table(&["Faucet", "Amount"]);
        let mut non_fungible_table = create_dynamic_table(&["Faucet ID", "Asset ID"]);
        for asset in account.vault().assets() {
            match asset.as_fungible() {
                Some(fungible_asset) => {
                    let faucet_id = fungible_asset.faucet_id();
                    let asset_amount = fungible_asset.amount();
                    let (faucet, amount) = match get_faucet_token_info(client, faucet_id).await {
                        Ok((symbol, decimals)) => {
                            (symbol.to_string(), base_units_to_tokens(asset_amount, decimals))
                        },
                        Err(_) => (faucet_id.prefix().to_hex(), asset_amount.as_u64().to_string()),
                    };
                    fungible_table.add_row(vec![faucet, amount]);
                },
                None => {
                    non_fungible_table
                        .add_row(vec![asset.faucet_id().to_hex(), asset.id().to_string()]);
                },
            }
        }

        println!("Fungible assets: ");
        println!("{fungible_table}\n");
        println!("Non fungible assets: ");
        println!("{non_fungible_table}\n");
    }

    // Storage Table
    {
        println!("Storage: \n");

        let mut table = create_dynamic_table(&["Slot Name", "Slot Type", "Value/Commitment"]);

        for slot in account.storage().header().slots() {
            let item = slot.value();

            // Last entry is reserved so I don't think the user cares about it. Also, to keep the
            // output smaller, if the [StorageSlot] is a value and it's 0 we assume it's not
            // initialized and skip it
            if slot.slot_type() == StorageSlotType::Value && item == [ZERO; 4].into() {
                continue;
            }

            let slot_type = match slot.slot_type() {
                StorageSlotType::Value => "Value",
                StorageSlotType::Map => "Map",
            };
            table.add_row(vec![slot.name().as_str(), slot_type, &item.to_hex()]);
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

/// Loads `account_id` as a [`PartialAccount`], falling back to fetching it from the network when
/// the client does not track it locally.
///
/// The partial account holds the full vault and only the storage slot headers. Storage map entries
/// are not loaded because `account show` shows only the map roots.
async fn load_partial_account<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
    rpc_config: &RpcConfig,
) -> Result<PartialAccount, CliError> {
    let account_error =
        |err| CliError::Account(err, format!("failed to build partial account {account_id}"));

    if let Some(account) = client.get_account(account_id).await? {
        let (id, vault, storage, code, nonce, seed) = account.into_parts();
        return PartialAccount::new(
            id,
            nonce,
            code,
            PartialStorage::new_minimal(&storage),
            PartialVault::new_full(vault),
            seed,
        )
        .map_err(account_error);
    }

    println!("Account {account_id} is not tracked by the client. Fetching from the network...");

    let rpc_client = VerifyingRpcClient::new(GrpcClient::new(
        &rpc_config.endpoint.clone().into(),
        rpc_config.timeout_ms,
    ));

    let fetch_error = |err| {
        CliError::Input(format!("Unable to fetch account {account_id} from the network: {err}"))
    };

    let (block_number, mut account_proof) = rpc_client
        .get_account(account_id, GetAccountRequest::new().with_vault(VaultFetch::Always))
        .await
        .map_err(fetch_error)?;

    if let Some(details) = account_proof.details_mut() {
        rpc_client
            .resolve_oversize_vault(account_id, block_number, details)
            .await
            .map_err(fetch_error)?;
    }

    let details = account_proof.into_details().ok_or(CliError::Input(format!(
        "Account {account_id} is private and not tracked by the client",
    )))?;

    let vault = AssetVault::new(&details.vault_details.assets)
        .map_err(|err| CliError::Input(format!("Invalid vault for account {account_id}: {err}")))?;
    let storage = PartialStorage::new(details.storage_details.header, []).map_err(account_error)?;

    PartialAccount::new(
        account_id,
        details.header.nonce(),
        details.code,
        storage,
        PartialVault::new_full(vault),
        None,
    )
    .map_err(account_error)
}

/// Prints a summary table with account information.
fn print_summary_table(account: &PartialAccount, network_id: NetworkId, kind: &AccountKind) {
    let mut table = create_dynamic_table(&["Account Information"]);
    table
        .load_preset(presets::UTF8_HORIZONTAL_ONLY)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth);

    table.add_row(vec![
        Cell::new("Address"),
        Cell::new(account_bech_32(account.id(), account.code(), network_id)),
    ]);
    table.add_row(vec![Cell::new("Account ID (hex)"), Cell::new(account.id().to_string())]);
    table.add_row(vec![
        Cell::new("Account Commitment"),
        Cell::new(account.to_commitment().to_string()),
    ]);
    table.add_row(vec![Cell::new("Kind"), Cell::new(kind.to_string())]);
    table.add_row(vec![Cell::new("Type"), Cell::new(account.id().account_type().to_string())]);
    table.add_row(vec![
        Cell::new("Code Commitment"),
        Cell::new(account.code().commitment().to_string()),
    ]);
    table.add_row(vec![Cell::new("Vault Root"), Cell::new(account.vault().root().to_string())]);
    table.add_row(vec![
        Cell::new("Storage Root"),
        Cell::new(account.storage().commitment().to_string()),
    ]);
    table.add_row(vec![
        Cell::new("Nonce"),
        Cell::new(account.nonce().as_canonical_u64().to_string()),
    ]);

    println!("{table}\n");
}

/// Reads the faucet's token symbol and decimals from its token config storage slot.
///
/// # Errors
/// Returns an error if the account is not tracked by the client, has no token config slot (i.e.
/// is not a fungible faucet), or the token config can't be decoded.
async fn get_faucet_token_info<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
) -> Result<(TokenSymbol, u8), CliError> {
    let token_config = client
        .account_reader(account_id)
        .get_storage_item(FungibleFaucet::token_config_slot().clone())
        .await?;

    decode_token_config(account_id, token_config)
}

/// Decodes the token symbol and decimals from the token config word of a faucet.
///
/// # Errors
/// Returns an error if the symbol or the decimals can't be decoded.
fn decode_token_config(
    account_id: AccountId,
    token_config: Word,
) -> Result<(TokenSymbol, u8), CliError> {
    // Token config word layout: `[token_supply, max_supply, decimals, symbol]` (see
    // `FungibleFaucet::token_config_slot_value`).
    let [_token_supply, _max_supply, decimals, symbol] = *token_config;
    let symbol = TokenSymbol::try_from(symbol).map_err(|err| {
        CliError::Input(format!("failed to decode token symbol of faucet {account_id}: {err}"))
    })?;
    let decimals = u8::try_from(decimals.as_canonical_u64()).map_err(|err| {
        CliError::Input(format!("failed to decode token decimals of faucet {account_id}: {err}"))
    })?;

    Ok((symbol, decimals))
}

/// Reads the token symbol of a non-fungible faucet from its symbol storage slot.
///
/// # Errors
/// Returns an error if the account is not tracked by the client, has no symbol slot (i.e. is not a
/// non-fungible faucet), or the symbol can't be decoded.
async fn get_non_fungible_faucet_symbol<AUTH>(
    client: &Client<AUTH>,
    account_id: AccountId,
) -> Result<TokenSymbol, CliError> {
    let symbol_word = client
        .account_reader(account_id)
        .get_storage_item(NonFungibleFaucet::symbol_slot().clone())
        .await?;

    decode_non_fungible_symbol(account_id, symbol_word)
}

/// Decodes the token symbol from the symbol slot word of a non-fungible faucet.
///
/// # Errors
/// Returns an error if the symbol can't be decoded.
fn decode_non_fungible_symbol(
    account_id: AccountId,
    symbol_word: Word,
) -> Result<TokenSymbol, CliError> {
    // Symbol slot word layout: `[symbol, 0, 0, 0]` (see `NonFungibleFaucet::symbol_slot_value`).
    TokenSymbol::try_from(symbol_word[0]).map_err(|err| {
        CliError::Input(format!("failed to decode token symbol of faucet {account_id}: {err}"))
    })
}

/// The kind of an account for display.
///
/// The on-chain `AccountType` only encodes account visibility (`public` / `private`), so the kind
/// is inferred from the components of the account.
enum AccountKind {
    FungibleFaucet(TokenSymbol),
    NonFungibleFaucet(TokenSymbol),
    Regular,
}

impl core::fmt::Display for AccountKind {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::FungibleFaucet(symbol) => write!(f, "Fungible faucet (token symbol: {symbol})"),
            Self::NonFungibleFaucet(symbol) => {
                write!(f, "Non-fungible faucet (token symbol: {symbol})")
            },
            Self::Regular => f.write_str("Regular"),
        }
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

fn account_bech_32(account_id: AccountId, code: &AccountCode, network_id: NetworkId) -> String {
    let mut address = Address::new(account_id);
    if account_code_has_basic_wallet(account_id, code) {
        address =
            address.with_routing_parameters(RoutingParameters::new(AddressInterface::BasicWallet));
    }

    address.encode(network_id)
}

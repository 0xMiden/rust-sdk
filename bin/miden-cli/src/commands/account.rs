use clap::Parser;
use comfy_table::{Cell, ContentArrangement, presets};
use miden_client::account::component::FungibleFaucet;
use miden_client::account::{
    Account,
    AccountCode,
    AccountId,
    AccountInterfaceExt,
    StorageSlotContent,
};
use miden_client::address::{Address, AddressInterface, NetworkId, RoutingParameters};
use miden_client::asset::Asset;
use miden_client::rpc::{GrpcClient, NodeRpcClient, VerifyingRpcClient};
use miden_client::transaction::{AccountComponentInterface, AccountInterface};
use miden_client::utils::base_units_to_tokens;
use miden_client::{Client, PrettyPrint, ZERO};

use crate::config::{CliConfig, RpcConfig};
use crate::errors::CliError;
use crate::utils::parse_account_id;
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
    /// When using --show, include the account code in the output.
    #[arg(long, requires = "show")]
    with_code: bool,
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
                show_account(&client, account_id, &cli_config.rpc, self.with_code).await?;
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
    with_code: bool,
) -> Result<(), CliError> {
    let account = if let Some(account) = client.get_account(account_id).await? {
        account
    } else {
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
        )))?
    };

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

    // Account code
    if with_code {
        println!("Code: \n");

        let mut table = create_dynamic_table(&["Code"]);
        table.add_row(vec![&account.code().to_pretty_string()]);
        println!("{table}");
    }

    Ok(())
}

// HELPERS
// ================================================================================================

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

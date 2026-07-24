use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use miden_client::account::{AccountId, FaucetMetadata};
use miden_client::address::{Address, AddressId};
use miden_client::asset::{Asset, FungibleAsset};
use miden_client::transaction::{ExecutedTransaction, InputNote};
use miden_client::utils::{base_units_to_tokens, tokens_to_base_units};
use miden_client::vm::MIN_STACK_DEPTH;
use miden_client::{Client, Felt, WORD_SIZE, Word};
use serde::Deserialize;

use super::{CLIENT_CONFIG_FILE_NAME, create_dynamic_table, get_account_with_id_prefix};
use crate::commands::account::DEFAULT_ACCOUNT_ID_KEY;
use crate::config::{CliConfig, get_global_miden_dir, get_local_miden_dir};
use crate::errors::CliError;

pub(crate) const SHARED_TOKEN_DOCUMENTATION: &str = "There are two accepted formats for the asset:
- `<AMOUNT>::<FAUCET_ID>` where `<AMOUNT>` is in the faucet base units.
- `<AMOUNT>::<TOKEN_SYMBOL>` where `<AMOUNT>` is a decimal number representing the quantity of
the token (specified to the precision allowed by the token's decimals), and `<TOKEN_SYMBOL>`
is a symbol tracked in the token symbol map file.

For example, `100::0xabcdef0123456789` or `1.23::TST`";

/// Returns a tracked Account ID matching a hex string or the default one defined in the Client
/// config.
pub(crate) async fn get_input_acc_id_by_prefix_or_default<AUTH>(
    client: &Client<AUTH>,
    account_id: Option<String>,
) -> Result<AccountId, CliError> {
    let account_id_str = if let Some(account_id_prefix) = account_id {
        account_id_prefix
    } else {
        client
            .get_setting(DEFAULT_ACCOUNT_ID_KEY.to_string())
            .await?
            .map(AccountId::to_hex)
            .ok_or(CliError::Input("No input account ID nor default account defined".to_string()))?
    };

    parse_account_id(client, &account_id_str).await
}

/// Parses a user provided account ID string and returns the corresponding `AccountId`.
///
/// `account_id` can fall into three categories:
///
/// - It's a hex prefix of an account ID of an account tracked by the client.
/// - It's a full hex account ID.
/// - It's a full bech32 account ID.
///
/// # Errors
///
/// - Will return a `IdPrefixFetchError` if the provided account ID string can't be parsed as an
///   `AccountId` and doesn't correspond to an account tracked by the client either.
pub(crate) async fn parse_account_id<AUTH>(
    client: &Client<AUTH>,
    account_id: &str,
) -> Result<AccountId, CliError> {
    if account_id.starts_with("0x") {
        if let Ok(account_id) = AccountId::from_hex(account_id) {
            return Ok(account_id);
        }

        Ok(get_account_with_id_prefix(client, account_id)
        .await
        .map_err(|_| CliError::Input(format!("Input account ID {account_id} is neither a valid Account ID nor a hex prefix of a known Account ID")))?
        .id())
    } else {
        let address = Address::decode(account_id)
            .map_err(|err| CliError::Input(format!("error parsing bech32 address: {err}")))?
            .1;
        match address.id() {
            AddressId::AccountId(account_id_address) => Ok(account_id_address),
            _ => Err(CliError::Input(format!(
                "Input account ID {address:?} is not an ID based address"
            ))),
        }
    }
}

/// Checks if either local or global configuration file exists.
pub(super) fn config_file_exists() -> Result<bool, CliError> {
    let local_miden_dir = get_local_miden_dir()?;
    if local_miden_dir.join(CLIENT_CONFIG_FILE_NAME).exists() {
        return Ok(true);
    }

    let global_miden_dir = get_global_miden_dir().map_err(|e| {
        CliError::Config(Box::new(e), "Failed to determine global config directory".to_string())
    })?;

    Ok(global_miden_dir.join(CLIENT_CONFIG_FILE_NAME).exists())
}

/// Returns the faucet metadata resolver using the config file.
pub fn load_faucet_metadata_resolver() -> Result<FaucetMetadataResolver, CliError> {
    let config = CliConfig::load()?;
    FaucetMetadataResolver::new(config.token_symbol_map_filepath)
}

/// Prints the effects of an executed transaction: input notes, output notes, storage value
/// changes, storage map changes, vault changes, and the nonce change.
pub async fn print_executed_transaction<AUTH>(
    client: &mut Client<AUTH>,
    executed_tx: &ExecutedTransaction,
) -> Result<(), CliError> {
    println!("The transaction will have the following effects:\n");

    let patch = executed_tx.account_patch();

    // INPUT NOTES
    let input_note_ids = executed_tx.input_notes().iter().map(InputNote::id).collect::<Vec<_>>();
    if input_note_ids.is_empty() {
        println!("No notes will be consumed.");
    } else {
        println!("The following notes will be consumed:");
        for input_note_id in input_note_ids {
            println!("\t- {}", input_note_id.to_hex());
        }
    }
    println!();

    // OUTPUT NOTES
    let output_notes: Vec<_> = executed_tx.output_notes().iter().collect();
    if output_notes.is_empty() {
        println!("No notes will be created as a result of this transaction.");
    } else {
        println!("{} notes will be created as a result of this transaction:", output_notes.len());
        for note in &output_notes {
            println!("\t- {}", note.id().to_hex());
        }
    }
    println!();

    // STORAGE VALUES
    if patch.storage().values().next().is_some() {
        let mut table = create_dynamic_table(&["Storage Slot", "New Value"]);
        for (slot, value_patch) in patch.storage().values() {
            let new_value =
                value_patch.value().map_or_else(|| "removed".to_string(), |v| v.to_hex());
            table.add_row(vec![slot.to_string(), new_value]);
        }
        println!("Storage changes:");
        println!("{table}");
    } else {
        println!("Account Storage will not be changed.");
    }

    // STORAGE MAPS
    if patch.storage().maps().next().is_some() {
        let mut table = create_dynamic_table(&["Storage Slot", "Map Key", "New Value"]);
        for (slot, map_patch) in patch.storage().maps() {
            for (key, value) in map_patch.entries().into_iter().flat_map(|e| e.as_map().iter()) {
                table.add_row(vec![slot.to_string(), Word::from(*key).to_hex(), value.to_hex()]);
            }
        }
        println!("Storage map changes:");
        println!("{table}");
    }

    // VAULT
    // The patch carries the new absolute value of each changed asset, cleared entries are listed as
    // removed.
    if patch.vault().is_empty() {
        println!("Account Vault will not be changed.");
    } else {
        let resolver = load_faucet_metadata_resolver()?;
        let mut table = create_dynamic_table(&["Asset Type", "Faucet ID", "New Amount"]);

        for asset in patch.vault().updated_assets() {
            match asset {
                Asset::Fungible(fungible) => {
                    let (faucet_fmt, amount_fmt) =
                        resolver.format_fungible_asset(client, &fungible).await?;
                    table.add_row(vec!["Fungible Asset", &faucet_fmt, &amount_fmt]);
                },
                Asset::NonFungible(non_fungible) => {
                    table.add_row(vec![
                        "Non Fungible Asset",
                        &non_fungible.faucet_id().prefix().to_hex(),
                        "1",
                    ]);
                },
            }
        }

        for asset_id in patch.vault().removed_asset_ids() {
            table.add_row(vec![
                "Removed Asset",
                &asset_id.faucet_id().prefix().to_hex(),
                "removed",
            ]);
        }

        println!("Vault changes:");
        println!("{table}");
    }

    // NONCE
    match patch.final_nonce() {
        Some(nonce) => println!("New account nonce: {nonce}."),
        None => println!("Account nonce will not be changed."),
    }

    Ok(())
}

/// Prints the output stack from `execute_program`.
///
/// If `expected_results` is `Some(n)`, prints the top `n` values. If `None`, prints up to the
/// last non-zero value so trailing zero-padding is hidden.
pub fn print_executed_program_stack(
    stack: &[Felt; MIN_STACK_DEPTH],
    expected_results: Option<usize>,
) {
    let count = match expected_results {
        Some(n) => n,
        None => stack.iter().rposition(|v| v.as_canonical_u64() != 0).map_or(0, |pos| pos + 1),
    };

    match count {
        0 => println!("\nResult: 0"),
        1 => println!("\nResult: {}", stack[0]),
        _ => {
            println!("\nResult ({count} values):");
            for (i, val) in stack.iter().enumerate().take(count) {
                println!("  [{i}]: {val}");
            }
        },
    }
}

/// Prints the output stack as four 4-felt words with their hex encoding.
pub fn print_executed_program_stack_hex_words(stack: &[Felt; MIN_STACK_DEPTH]) {
    let last_word_start = MIN_STACK_DEPTH - WORD_SIZE;
    println!("Output stack:");
    for word_idx in (0..MIN_STACK_DEPTH).step_by(WORD_SIZE) {
        let word_idx_end = word_idx + WORD_SIZE - 1;
        let prefix = if word_idx == last_word_start {
            "└──"
        } else {
            "├──"
        };
        let word = [stack[word_idx], stack[word_idx + 1], stack[word_idx + 2], stack[word_idx + 3]];
        println!(
            "{prefix} {word_idx:2} - {word_idx_end:2}: {word:?} ({})",
            Word::from(word).to_hex()
        );
    }
}

// FAUCET METADATA RESOLVER
// ================================================================================================

/// Raw TOML row as written by the user. The `id` is a bech32 address.
#[derive(Debug, Deserialize)]
struct RawFaucetEntry {
    pub id: String,
    pub decimals: u8,
}

/// Parsed entry — the `id` string has been normalized into a typed `AccountId`.
#[derive(Debug, Clone)]
struct FaucetTomlEntry {
    pub account_id: AccountId,
    pub decimals: u8,
}

/// Resolves faucet display metadata (symbol + decimals) for a given faucet `AccountId`.
///
/// Lookup walks three sources in priority order:
///
/// 1. The user's TOML symbol map (bech32 `id`).
/// 2. The client's settings store, populated from previous RPC fetches.
/// 3. A fresh RPC fetch from the network. Successful fetches are persisted back to the settings
///    store.
#[derive(Debug)]
pub struct FaucetMetadataResolver {
    toml: BTreeMap<String, FaucetTomlEntry>,
}

impl FaucetMetadataResolver {
    /// Creates a new instance of the [`FaucetMetadataResolver`] by loading the token symbol map
    /// file from the specified `token_symbol_map_filepath`. If the file doesn't exist, an empty
    /// map is created.
    pub fn new(token_symbol_map_filepath: PathBuf) -> Result<Self, CliError> {
        let raw: BTreeMap<String, RawFaucetEntry> =
            match std::fs::read_to_string(token_symbol_map_filepath) {
                Ok(content) => toml::from_str(&content).map_err(|err| {
                    CliError::Config(
                        Box::new(err),
                        "Failed to parse token_symbol_map file".to_string(),
                    )
                })?,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
                Err(err) => {
                    return Err(CliError::Config(
                        Box::new(err),
                        "Failed to read token_symbol_map file".to_string(),
                    ));
                },
            };

        let mut parsed: BTreeMap<String, FaucetTomlEntry> = BTreeMap::new();
        let mut seen: BTreeSet<AccountId> = BTreeSet::new();
        for (symbol, entry) in raw {
            let account_id = parse_id_string(&entry.id).map_err(|err| {
                CliError::Config(
                    err.into(),
                    format!("Failed to parse `id` for token symbol {symbol}"),
                )
            })?;
            if !seen.insert(account_id) {
                return Err(CliError::Config(
                    format!(
                        "Faucet ID {} appears more than once in the token symbol map",
                        account_id.to_hex(),
                    )
                    .into(),
                    "Failed to parse token_symbol_map file".to_string(),
                ));
            }
            parsed.insert(symbol, FaucetTomlEntry { account_id, decimals: entry.decimals });
        }

        Ok(Self { toml: parsed })
    }

    /// Looks up `(symbol, decimals)` for a faucet using only local sources: the TOML map and the
    /// settings store. Returns `None` without performing any network request.
    pub async fn resolve_local<AUTH>(
        &self,
        client: &Client<AUTH>,
        faucet_id: AccountId,
    ) -> Result<Option<FaucetMetadata>, CliError> {
        // 1) TOML
        if let Some((symbol, decimals)) = self.lookup_toml(&faucet_id) {
            return Ok(Some(FaucetMetadata { symbol, decimals }));
        }
        // 2) settings store
        let setting_key = faucet_metadata_setting_key(faucet_id);
        Ok(client.get_setting::<FaucetMetadata>(setting_key).await?)
    }

    /// Looks up `(symbol, decimals)` for a faucet, walking TOML → settings store → RPC fetch.
    /// On RPC success, the result is persisted to the settings store.
    pub async fn resolve<AUTH>(
        &self,
        client: &mut Client<AUTH>,
        faucet_id: AccountId,
    ) -> Result<Option<FaucetMetadata>, CliError> {
        // 1) & 2) local sources (TOML + settings store)
        if let Some(meta) = self.resolve_local(client, faucet_id).await? {
            return Ok(Some(meta));
        }
        // 3) RPC fetch
        let setting_key = faucet_metadata_setting_key(faucet_id);
        match client.fetch_remote_token_metadata(faucet_id).await {
            Ok(Some(meta)) => {
                if let Err(err) = client.set_setting(setting_key, meta.clone()).await {
                    tracing::warn!(
                        "failed to persist faucet metadata for {}: {err}",
                        faucet_id.to_hex(),
                    );
                }
                Ok(Some(meta))
            },
            Ok(None) => Ok(None),
            Err(err) => {
                tracing::warn!("failed to fetch faucet metadata for {}: {err}", faucet_id.to_hex());
                Ok(None)
            },
        }
    }

    /// Formats a fungible asset using [`Self::resolve`]. On miss, returns
    /// `(<bech32 faucet address>, <base-unit amount>)`.
    pub async fn format_fungible_asset<AUTH>(
        &self,
        client: &mut Client<AUTH>,
        asset: &FungibleAsset,
    ) -> Result<(String, String), CliError> {
        if let Some(meta) = self.resolve(client, asset.faucet_id()).await? {
            return Ok((meta.symbol, base_units_to_tokens(asset.amount(), meta.decimals)));
        }
        let network_id = client.network_id().await?;
        let address_str = Address::new(asset.faucet_id()).encode(network_id);
        Ok((address_str, asset.amount().to_string()))
    }

    /// Parses a string representing a [`FungibleAsset`]. There are two accepted formats for the
    /// string:
    /// - `<AMOUNT>::<FAUCET_ID>` where `<AMOUNT>` is in the faucet base units and `<FAUCET_ID>` is
    ///   the faucet's account ID.
    /// - `<AMOUNT>::<FAUCET_ADDRESS>` where `<AMOUNT>` is in the faucet base units and
    ///   `<FAUCET_ADDRESS>` is the faucet address.
    /// - `<AMOUNT>::<TOKEN_SYMBOL>` where `<AMOUNT>` is a decimal number representing the quantity
    ///   of the token (specified to the precision allowed by the token's decimals), and
    ///   `<TOKEN_SYMBOL>` is a symbol tracked in the token symbol map file.
    ///
    /// Some examples of valid `arg` values are `100::mlcl1qru2e5yvx40ndgqqqzusrryr0ucyd0uj`,
    /// `100::0xabcdef0123456789` and `1.23::TST`.
    ///
    /// # Errors
    ///
    /// Will return an error if:
    /// - The provided `arg` doesn't match one of the expected formats.
    /// - A faucet ID was provided but the amount isn't in base units.
    /// - The amount has more than the allowed number of decimals.
    /// - The token symbol isn't present in the token symbol map file.
    pub async fn parse_fungible_asset<AUTH>(
        &self,
        client: &Client<AUTH>,
        arg: &str,
    ) -> Result<FungibleAsset, CliError> {
        let (amount, asset) = arg.split_once("::").ok_or(CliError::Parse(
            "separator `::` not found".into(),
            "Failed to parse amount and asset".to_string(),
        ))?;
        let (faucet_id, amount) = if let Ok(id) = parse_account_id(client, asset).await {
            let amount = amount
                .parse::<u64>()
                .map_err(|err| CliError::Parse(err.into(), "Failed to parse u64".to_string()))?;
            (id, amount)
        } else {
            let entry = self.toml.get(asset).ok_or(CliError::Config(
                "Token symbol not found in the map file".to_string().into(),
                asset.to_string(),
            ))?;
            let amount = tokens_to_base_units(amount, entry.decimals).map_err(|err| {
                CliError::Parse(err.into(), "Failed to parse tokens to base units".to_string())
            })?;
            (entry.account_id, amount.as_u64())
        };

        FungibleAsset::new(faucet_id, amount).map_err(CliError::Asset)
    }

    fn lookup_toml(&self, faucet_id: &AccountId) -> Option<(String, u8)> {
        self.toml
            .iter()
            .find(|(_, entry)| &entry.account_id == faucet_id)
            .map(|(symbol, entry)| (symbol.clone(), entry.decimals))
    }
}

/// Settings key prefix under which faucet display metadata is persisted.
const FAUCET_METADATA_SETTING_PREFIX: &str = "faucet_metadata:";

/// Returns the settings-store key under which the metadata for `faucet_id` is persisted.
fn faucet_metadata_setting_key(faucet_id: AccountId) -> String {
    format!("{FAUCET_METADATA_SETTING_PREFIX}{}", faucet_id.to_hex())
}

/// Parses an `id` string from the TOML as a bech32 address.
fn parse_id_string(id: &str) -> Result<AccountId, String> {
    let (_, address) = Address::decode(id)
        .map_err(|err| format!("`{id}` is not a valid bech32 address: {err}"))?;
    if let AddressId::AccountId(account_id) = address.id() {
        return Ok(account_id);
    }
    Err(format!("address `{id}` does not encode an account ID"))
}

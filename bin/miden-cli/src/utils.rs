use std::collections::{BTreeMap, BTreeSet};
use std::num::ParseIntError;
use std::path::PathBuf;
use std::sync::Mutex;

use miden_client::account::standards::faucets::FungibleFaucet;
use miden_client::account::{AccountId, FaucetMetadata};
use miden_client::address::{Address, AddressId, NetworkId};
use miden_client::asset::{Asset, AssetAmount, AssetId, FungibleAsset};
use miden_client::keystore::Keystore;
use miden_client::transaction::ExecutedTransaction;
use miden_client::utils::{Deserializable, DeserializationError, Serializable};
use miden_client::vm::{ExecutionError, MIN_STACK_DEPTH, OperationError, error_code_from_msg};
use miden_client::{
    AssetError,
    Client,
    ClientError,
    Felt,
    TransactionExecutorError,
    WORD_SIZE,
    Word,
};
use serde::Deserialize;

use super::{CLIENT_CONFIG_FILE_NAME, create_dynamic_table, get_account_with_id_prefix};
use crate::commands::account::{
    get_default_account_id,
    get_faucet_token_info,
    get_non_fungible_faucet_symbol,
};
use crate::config::{CliConfig, get_global_miden_dir, get_local_miden_dir};
use crate::errors::CliError;
use crate::notes::display::{print_consumed_notes, print_output_notes};

/// Placeholder for a value that is not available or not set.
pub(crate) const NO_VALUE: &str = "-";

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
        get_default_account_id(client)
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
/// - It's a full bech32 address.
///
/// An address encodes the network it belongs to. An address from another network refers to another
/// chain, so it is rejected.
///
/// # Errors
///
/// - Will return a `IdPrefixFetchError` if the provided account ID string can't be parsed as an
///   `AccountId` and doesn't correspond to an account tracked by the client either.
/// - Will return a `CliError::Input` if the address belongs to another network.
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
        let (address_network_id, address) = Address::decode(account_id)
            .map_err(|err| CliError::Input(format!("error parsing bech32 address: {err}")))?;
        validate_network_eq(&address_network_id, &configured_network_id()?)?;
        match address.id() {
            AddressId::AccountId(account_id_address) => Ok(account_id_address),
            _ => Err(CliError::Input(format!(
                "Input account ID {address:?} is not an ID based address"
            ))),
        }
    }
}

/// Rejects an address that belongs to a network other than the configured one.
pub(crate) fn validate_network_eq(
    address_network_id: &NetworkId,
    client_network_id: &NetworkId,
) -> Result<(), CliError> {
    if address_network_id != client_network_id {
        return Err(CliError::Input(format!(
            "Address network `{address_network_id}` does not match configured network `{client_network_id}`",
        )));
    }

    Ok(())
}

/// Returns true if the string can only be an account ID or an address, and not a token symbol.
fn is_account_identifier(asset: &str) -> bool {
    asset.starts_with("0x") || Address::decode(asset).is_ok()
}

/// Splits a `<ACCOUNT_ID>[:<PROCEDURE>]` target into its account ID and procedure parts.
///
/// Account IDs (hex or bech32) never contain a colon, so the first one separates the two. The
/// procedure is `None` when the target carries no colon; commands that require one reject that case
/// themselves.
pub(crate) fn split_procedure_target(target: &str) -> (&str, Option<&str>) {
    match target.split_once(':') {
        Some((account_id, procedure)) => (account_id, Some(procedure)),
        None => (target, None),
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
    let network_id = config.network_id()?;
    FaucetMetadataResolver::new(config.token_symbol_map_filepath, &network_id)
}

/// Returns the network ID of the configured network. See [`CliConfig::network_id`].
pub(crate) fn configured_network_id() -> Result<NetworkId, CliError> {
    CliConfig::load()?.network_id()
}

/// Prints the effects of an executed transaction: input notes, output notes, storage value changes,
/// storage map changes, vault changes, and the nonce change.
pub async fn print_executed_transaction<AUTH: Keystore + Sync>(
    client: &Client<AUTH>,
    executed_tx: &ExecutedTransaction,
) -> Result<(), CliError> {
    println!("The transaction will have the following effects:\n");

    let patch = executed_tx.account_patch();
    let input_notes = executed_tx.input_notes();
    let output_notes = executed_tx.output_notes();
    let needs_metadata =
        !input_notes.is_empty() || !output_notes.is_empty() || !patch.vault().is_empty();
    let resolver = needs_metadata.then(load_faucet_metadata_resolver).transpose()?;

    // INPUT NOTES
    if input_notes.is_empty() {
        println!("No notes will be consumed.");
    } else {
        let resolver = resolver.as_ref().expect("input notes require a metadata resolver");
        print_consumed_notes(client, resolver, input_notes).await?;
    }

    // OUTPUT NOTES
    if output_notes.is_empty() {
        // The table of output notes starts with a blank line. This message does the same.
        println!("\nNo notes will be created as a result of this transaction.");
    } else {
        let resolver = resolver.as_ref().expect("output notes require a metadata resolver");
        print_output_notes(client, resolver, output_notes, false).await?;
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
    //
    // The patch carries the new absolute value of each changed asset, cleared entries are listed as
    // removed.
    if patch.vault().is_empty() {
        println!("Account Vault will not be changed.");
    } else {
        let resolver = resolver.as_ref().expect("vault changes require a metadata resolver");
        let mut fungible_table = create_dynamic_table(&["Faucet ID", "New Amount"]);
        let mut non_fungible_table = create_dynamic_table(&["Faucet ID", "Asset ID", "Change"]);

        for asset in patch.vault().updated_assets() {
            match asset.as_fungible() {
                Some(fungible) => {
                    let metadata = resolver.resolve(client, fungible.faucet_id()).await?;
                    let (faucet, amount) = FormattedAsset::fungible_parts(
                        &fungible,
                        metadata.as_ref(),
                        resolver.network_id(),
                    );
                    fungible_table.add_row(vec![faucet, amount]);
                },
                None => {
                    non_fungible_table.add_row(vec![
                        asset.faucet_id().to_hex(),
                        asset.id().to_string(),
                        "added".to_string(),
                    ]);
                },
            }
        }

        for asset_id in patch.vault().removed_asset_ids() {
            if asset_id.composition().is_none() {
                non_fungible_table.add_row(vec![
                    asset_id.faucet_id().to_hex(),
                    asset_id.to_string(),
                    "removed".to_string(),
                ]);
            } else {
                fungible_table.add_row(vec![asset_id.faucet_id().to_hex(), "removed".to_string()]);
            }
        }

        if !fungible_table.is_empty() {
            println!("Fungible asset changes:");
            println!("{fungible_table}");
        }
        if !non_fungible_table.is_empty() {
            println!("Non fungible asset changes:");
            println!("{non_fungible_table}");
        }
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
/// If `expected_results` is `Some(n)`, prints the top `n` values. If `None`, prints up to the last
/// non-zero value so trailing zero-padding is hidden.
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

// TOKEN AMOUNT CONVERSION
// ================================================================================================

/// Converts an amount in the faucet base units to the token's decimals.
///
/// This is meant for display purposes only.
pub(crate) fn base_units_to_tokens(units: AssetAmount, decimals: u8) -> String {
    let units_str = units.as_u64().to_string();
    let len = units_str.len();

    if decimals == 0 {
        return units_str;
    }

    if decimals as usize >= len {
        // Handle cases where the number of decimals is greater than the length of units
        "0.".to_owned() + &"0".repeat(decimals as usize - len) + &units_str
    } else {
        // Insert the decimal point at the correct position
        let integer_part = &units_str[..len - decimals as usize];
        let fractional_part = &units_str[len - decimals as usize..];
        format!("{integer_part}.{fractional_part}")
    }
}

/// Errors that can occur when parsing a token represented as a decimal number in a string into base
/// units.
#[derive(thiserror::Error, Debug)]
pub(crate) enum TokenParseError {
    #[error("Number of decimals {0} must be less than or equal to {max_decimals}", max_decimals = FungibleFaucet::MAX_DECIMALS)]
    MaxDecimals(u8),
    #[error("More than one decimal point")]
    MultipleDecimalPoints,
    #[error("Failed to parse u64")]
    ParseU64(#[source] ParseIntError),
    #[error("Amount has more than {0} decimal places")]
    TooManyDecimals(u8),
    #[error("Amount is not a valid asset amount")]
    InvalidAmount(#[source] AssetError),
}

/// Converts a decimal number, represented as a string, into an integer by shifting the decimal
/// point to the right by a specified number of decimal places.
pub(crate) fn tokens_to_base_units(
    decimal_str: &str,
    n_decimals: u8,
) -> Result<AssetAmount, TokenParseError> {
    if n_decimals > FungibleFaucet::MAX_DECIMALS {
        return Err(TokenParseError::MaxDecimals(n_decimals));
    }

    // Split the string on the decimal point
    let parts: Vec<&str> = decimal_str.split('.').collect();

    if parts.len() > 2 {
        return Err(TokenParseError::MultipleDecimalPoints);
    }

    // Validate that the parts are valid numbers
    for part in &parts {
        part.parse::<u64>().map_err(TokenParseError::ParseU64)?;
    }

    let integer_part = parts[0];

    // Trailing zeros carry no value and would change the decimal count.
    let mut fractional_part = if parts.len() > 1 {
        parts[1].trim_end_matches('0').to_string()
    } else {
        String::new()
    };

    // Check if the fractional part has more than N decimals
    if fractional_part.len() > n_decimals.into() {
        return Err(TokenParseError::TooManyDecimals(n_decimals));
    }

    // Add extra zeros if the fractional part is shorter than N decimals
    while fractional_part.len() < n_decimals.into() {
        fractional_part.push('0');
    }

    // Combine the integer and padded fractional part
    let combined = format!("{}{}", integer_part, &fractional_part[0..n_decimals.into()]);

    // Convert the combined string to an integer
    let units = combined.parse::<u64>().map_err(TokenParseError::ParseU64)?;

    AssetAmount::new(units).map_err(TokenParseError::InvalidAmount)
}

// FAUCET METADATA RESOLVER
// ================================================================================================

/// Raw TOML row as written by the user.
#[derive(Debug, Deserialize)]
struct RawFaucetEntry {
    pub address: String,
    pub decimals: u8,
}

/// Parsed entry — the address string has been normalized into a typed `AccountId`.
#[derive(Debug, Clone)]
struct FaucetTomlEntry {
    pub account_id: AccountId,
    pub decimals: u8,
}

/// Resolves faucet display metadata (symbol + decimals) for a given faucet `AccountId`.
///
/// Lookup walks four sources in priority order:
///
/// 1. The user's TOML symbol map (bech32 `address`).
/// 2. The client's settings store, populated from previous RPC fetches.
/// 3. The tracked faucet account's storage.
/// 4. A fresh RPC fetch from the network. Successful fetches are persisted back to the settings
///    store.
#[derive(Debug)]
pub struct FaucetMetadataResolver {
    toml: BTreeMap<String, FaucetTomlEntry>,
    /// Holds the outcome of every faucet lookup for the lifetime of the resolver. It caches misses
    /// as well as hits, so several assets from the same untracked faucet cause at most one RPC
    /// fetch.
    cache: Mutex<BTreeMap<AccountId, Option<FaucetMetadata>>>,
    /// Holds the outcome of every non-fungible faucet symbol lookup, misses included, for the
    /// lifetime of the resolver.
    non_fungible_symbol_cache: Mutex<BTreeMap<AccountId, Option<String>>>,
    network_id: NetworkId,
}

/// The faucet details found for an asset. A fungible asset has `metadata`. A non-fungible asset has
/// `non_fungible_symbol`. A value is `None` when the lookup finds nothing.
#[derive(Debug, Default)]
pub struct ResolvedFaucet {
    pub metadata: Option<FaucetMetadata>,
    pub non_fungible_symbol: Option<String>,
}

/// An asset formatted for display in a CLI table.
pub enum FormattedAsset {
    Fungible {
        /// The token symbol when it is known. Otherwise, the faucet address.
        faucet: String,
        /// The token amount.
        amount: String,
    },
    NonFungible {
        /// The bech32 address of the faucet.
        address: String,
        /// The token symbol of the faucet when it is known.
        symbol: Option<String>,
        /// The asset ID.
        asset_id: AssetId,
    },
}

impl FormattedAsset {
    /// Formats an asset with the faucet details found for it. It does not look up anything.
    ///
    /// A fungible asset without `metadata` shows the bech32 faucet address and the amount in base
    /// units.
    pub fn new(asset: &Asset, resolved: &ResolvedFaucet, network_id: &NetworkId) -> Self {
        match asset.as_fungible() {
            Some(fungible) => {
                let (faucet, amount) =
                    Self::fungible_parts(&fungible, resolved.metadata.as_ref(), network_id);
                Self::Fungible { faucet, amount }
            },
            None => Self::NonFungible {
                address: Address::new(asset.faucet_id()).encode(network_id.clone()),
                symbol: resolved.non_fungible_symbol.clone(),
                asset_id: asset.id(),
            },
        }
    }

    /// Returns `(<faucet>, <amount>)` for a fungible asset. See [`Self::new`].
    pub fn fungible_parts(
        asset: &FungibleAsset,
        metadata: Option<&FaucetMetadata>,
        network_id: &NetworkId,
    ) -> (String, String) {
        match metadata {
            Some(meta) => {
                (meta.symbol.clone(), base_units_to_tokens(asset.amount(), meta.decimals))
            },
            None => (
                Address::new(asset.faucet_id()).encode(network_id.clone()),
                asset.amount().to_string(),
            ),
        }
    }
}

/// Renders a fungible asset as its amount and faucet, and a non-fungible asset as its faucet and
/// asset ID, on one line. A non-fungible asset shows its token symbol when it is known. Otherwise,
/// it shows the faucet address.
impl core::fmt::Display for FormattedAsset {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Fungible { faucet, amount } => write!(f, "{amount} {faucet}"),
            Self::NonFungible {
                symbol: Some(symbol), address, asset_id, ..
            } => {
                write!(f, "{symbol} (non-fungible, faucet {address}, asset ID {asset_id})")
            },
            Self::NonFungible { symbol: None, address, asset_id, .. } => {
                write!(f, "{address} (non-fungible, asset ID {asset_id})")
            },
        }
    }
}

impl FaucetMetadataResolver {
    /// Creates a new instance of the [`FaucetMetadataResolver`] by loading the token symbol map
    /// file from the specified `token_symbol_map_filepath`. If the file doesn't exist, an empty map
    /// is created.
    ///
    /// Every entry must hold an address of the `network_id` network. An entry of another network
    /// names a faucet on another chain, so it is rejected when the map is loaded.
    pub fn new(
        token_symbol_map_filepath: PathBuf,
        network_id: &NetworkId,
    ) -> Result<Self, CliError> {
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
            let account_id = parse_address(&entry.address, network_id).map_err(|err| {
                CliError::Config(
                    err.into(),
                    format!("Failed to parse `address` for token symbol {symbol}"),
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

        Ok(Self {
            toml: parsed,
            cache: Mutex::new(BTreeMap::new()),
            non_fungible_symbol_cache: Mutex::new(BTreeMap::new()),
            network_id: network_id.clone(),
        })
    }

    /// Returns the network of the token symbol map.
    pub fn network_id(&self) -> &NetworkId {
        &self.network_id
    }

    /// Looks up faucet metadata in the TOML map, settings store, and tracked account, in that
    /// order. Does not make a network request.
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
        if let Some(bytes) = client.get_setting(setting_key).await? {
            let meta = faucet_metadata_from_bytes(&bytes).map_err(|err| {
                CliError::Parse(
                    err.into(),
                    "Failed to parse the stored faucet metadata".to_string(),
                )
            })?;
            return Ok(Some(meta));
        }
        // 3) tracked faucet account
        Ok(get_faucet_token_info(client, faucet_id)
            .await
            .ok()
            .map(|(symbol, decimals)| FaucetMetadata { symbol: symbol.to_string(), decimals }))
    }

    /// Looks up faucet metadata locally, then through RPC. Persists successful RPC results.
    pub async fn resolve<AUTH>(
        &self,
        client: &Client<AUTH>,
        faucet_id: AccountId,
    ) -> Result<Option<FaucetMetadata>, CliError> {
        // 0) in-memory cache. It also holds misses, so an untracked faucet is fetched at most once.
        // The lock is scoped so the guard drops before the `await` below.
        {
            let cache = self.cache.lock().expect("faucet metadata cache mutex is poisoned");
            if let Some(cached) = cache.get(&faucet_id) {
                return Ok(cached.clone());
            }
        }

        // A transient RPC error is not cached. A later lookup can then retry instead of reading a
        // cached miss.
        let resolved = match self.resolve_uncached(client, faucet_id).await {
            Ok(resolved) => resolved,
            Err(err) => {
                tracing::warn!("failed to fetch faucet metadata for {}: {err}", faucet_id.to_hex());
                return Ok(None);
            },
        };

        self.cache
            .lock()
            .expect("faucet metadata cache mutex is poisoned")
            .insert(faucet_id, resolved.clone());
        Ok(resolved)
    }

    /// Runs the lookup without the in-memory cache. Returns RPC errors so they are not cached as
    /// misses.
    async fn resolve_uncached<AUTH>(
        &self,
        client: &Client<AUTH>,
        faucet_id: AccountId,
    ) -> Result<Option<FaucetMetadata>, CliError> {
        if let Some(meta) = self.resolve_local(client, faucet_id).await? {
            return Ok(Some(meta));
        }
        let setting_key = faucet_metadata_setting_key(faucet_id);
        let Some(meta) = client.fetch_remote_token_metadata(faucet_id).await? else {
            return Ok(None);
        };
        if let Err(err) = client.set_setting(setting_key, faucet_metadata_to_bytes(&meta)).await {
            tracing::warn!("failed to persist faucet metadata for {}: {err}", faucet_id.to_hex());
        }
        Ok(Some(meta))
    }

    /// Looks up the faucet details of an asset. A fungible asset is resolved through
    /// [`Self::resolve`]. A non-fungible asset is resolved through
    /// [`Self::resolve_non_fungible_symbol`].
    pub async fn resolve_asset<AUTH>(
        &self,
        client: &Client<AUTH>,
        asset: &Asset,
    ) -> Result<ResolvedFaucet, CliError> {
        match asset.as_fungible() {
            Some(fungible) => Ok(ResolvedFaucet {
                metadata: self.resolve(client, fungible.faucet_id()).await?,
                non_fungible_symbol: None,
            }),
            None => Ok(ResolvedFaucet {
                metadata: None,
                non_fungible_symbol: self
                    .resolve_non_fungible_symbol(client, asset.faucet_id())
                    .await,
            }),
        }
    }

    /// Looks up the token symbol of a non-fungible faucet. It reads the storage of a faucet that
    /// the client tracks, and otherwise fetches the symbol of a public faucet from the network.
    /// Returns `None` when the symbol is not found.
    async fn resolve_non_fungible_symbol<AUTH>(
        &self,
        client: &Client<AUTH>,
        faucet_id: AccountId,
    ) -> Option<String> {
        // The lock is scoped so the guard drops before the `await` below.
        {
            let cache = self
                .non_fungible_symbol_cache
                .lock()
                .expect("non-fungible symbol cache mutex is poisoned");
            if let Some(cached) = cache.get(&faucet_id) {
                return cached.clone();
            }
        }

        let symbol = if let Ok(symbol) = get_non_fungible_faucet_symbol(client, faucet_id).await {
            Some(symbol.to_string())
        } else {
            // A transient RPC error is not cached. A later lookup can then retry.
            match client.fetch_remote_non_fungible_faucet_symbol(faucet_id).await {
                Ok(symbol) => symbol.map(|symbol| symbol.to_string()),
                Err(_) => {
                    return None;
                },
            }
        };

        self.non_fungible_symbol_cache
            .lock()
            .expect("non-fungible symbol cache mutex is poisoned")
            .insert(faucet_id, symbol.clone());
        symbol
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
        let (faucet_id, amount) = match parse_account_id(client, asset).await {
            Ok(faucet_id) => {
                let amount = amount.parse::<u64>().map_err(|err| {
                    CliError::Parse(err.into(), "Failed to parse u64".to_string())
                })?;
                (faucet_id, amount)
            },
            // A token symbol is never an account ID or an address, so the token symbol map cannot
            // resolve this asset. Report why the account ID is invalid.
            Err(err) if is_account_identifier(asset) => return Err(err),
            Err(_) => {
                let entry = self.toml.get(asset).ok_or(CliError::Config(
                    "Token symbol not found in the map file".to_string().into(),
                    asset.to_string(),
                ))?;
                let amount = tokens_to_base_units(amount, entry.decimals).map_err(|err| {
                    CliError::Parse(err.into(), "Failed to parse tokens to base units".to_string())
                })?;
                (entry.account_id, amount.as_u64())
            },
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

/// Encodes `meta` for the settings store as its `(symbol, decimals)` pair.
fn faucet_metadata_to_bytes(meta: &FaucetMetadata) -> Vec<u8> {
    (&meta.symbol, meta.decimals).to_bytes()
}

/// Decodes faucet metadata that [`faucet_metadata_to_bytes`] encoded.
fn faucet_metadata_from_bytes(bytes: &[u8]) -> Result<FaucetMetadata, DeserializationError> {
    let (symbol, decimals) = <(String, u8)>::read_from_bytes(bytes)?;
    Ok(FaucetMetadata { symbol, decimals })
}

/// Parses a bech32 address from the token symbol map and checks that it belongs to `network_id`.
fn parse_address(address_str: &str, network_id: &NetworkId) -> Result<AccountId, String> {
    let (address_network_id, address) = Address::decode(address_str)
        .map_err(|err| format!("`{address_str}` is not a valid bech32 address: {err}"))?;
    validate_network_eq(&address_network_id, network_id).map_err(|err| err.to_string())?;
    if let AddressId::AccountId(account_id) = address.id() {
        return Ok(account_id);
    }
    Err(format!("address `{address_str}` does not encode an account ID"))
}

// EMPTY TRANSACTION
// ================================================================================================

/// The transaction kernel's assertion message for a transaction that changes nothing.
///
/// The message is hashed into the error code the assertion carries, and compared against it. It
/// only decides whether an explanatory line is printed: if the kernel ever rewords it, the line
/// stops appearing and nothing else changes.
const EMPTY_TRANSACTION_ASSERTION: &str =
    "executed transaction neither changed the account state, nor consumed any notes";

/// The line printed for a transaction that the kernel rejected because it changed nothing.
const EMPTY_TRANSACTION_REPORT: &str = "The transaction was rejected because it did not change the \
     account state and did not consume any notes.";

/// Returns true when `error` is the transaction kernel's rejection of a transaction that neither
/// changed the account state nor consumed a note.
pub(crate) fn is_empty_transaction_error(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::TransactionExecutorError(
            TransactionExecutorError::TransactionProgramExecutionFailed(
                ExecutionError::OperationError {
                    err: OperationError::FailedAssertion { err_code, .. },
                    ..
                },
            ),
        ) if *err_code == error_code_from_msg(EMPTY_TRANSACTION_ASSERTION)
    )
}

/// Builds the error the executor raises for a failed assertion with the message `msg`.
#[cfg(test)]
fn assertion_error(msg: &str) -> ClientError {
    let assertion = OperationError::FailedAssertion {
        err_code: error_code_from_msg(msg),
        err_msg: None,
    }
    .with_context();
    ClientError::TransactionExecutorError(
        TransactionExecutorError::TransactionProgramExecutionFailed(assertion),
    )
}

/// Builds the error the kernel raises for a transaction that changes nothing.
#[cfg(test)]
pub(crate) fn empty_transaction_error() -> ClientError {
    assertion_error(EMPTY_TRANSACTION_ASSERTION)
}

/// Prints why the kernel rejected a transaction that changed nothing.
pub(crate) fn print_empty_transaction_report() {
    println!("\n{EMPTY_TRANSACTION_REPORT}");
}

// DEBUG ADAPTER HINT
// ================================================================================================

/// The help text for a transaction program that failed in `exec` or `consume-notes`.
const DEBUG_ADAPTER_HINT: &str = "Re-run the command with `--start-debug-adapter <ADDR>` and \
     connect a DAP client, such as `miden-debug`, to step through the transaction and find the \
     instruction that failed. See \
     https://docs.miden.xyz/builder/tools/clients/rust-client/debugging";

/// The help text for the same failure when the CLI is built without the `dap` feature.
const DEBUG_ADAPTER_BUILD_HINT: &str = "Build the CLI with `--features dap`, re-run the command \
     with `--start-debug-adapter <ADDR>` and connect a DAP client, such as `miden-debug`, to step \
     through the transaction and find the instruction that failed. See \
     https://docs.miden.xyz/builder/tools/clients/rust-client/debugging";

/// Returns the debug adapter hint when the transaction program failed during execution.
pub(crate) fn debug_adapter_hint(error: &ClientError) -> Option<String> {
    match error {
        ClientError::TransactionExecutorError(
            TransactionExecutorError::TransactionProgramExecutionFailed(_),
        ) => {
            let hint = if cfg!(feature = "dap") {
                DEBUG_ADAPTER_HINT
            } else {
                DEBUG_ADAPTER_BUILD_HINT
            };
            Some(hint.to_string())
        },
        _ => None,
    }
}

/// Sets the debug adapter hint on a client error when the transaction program failed.
pub(crate) fn with_debug_adapter_hint(error: CliError) -> CliError {
    match error {
        CliError::Client { error, help } => {
            let help = debug_adapter_hint(&error).or(help);
            CliError::Client { error, help }
        },
        other => other,
    }
}

// ECDSA PUBLIC KEY PARSING
// ================================================================================================

/// Byte length of a SEC1-compressed secp256k1 public key (parity prefix plus x coordinate).
pub(crate) const ECDSA_COMPRESSED_KEY_BYTES: usize = 33;
/// Byte length of a SEC1-uncompressed secp256k1 public key (`0x04` prefix plus both coordinates).
pub(crate) const ECDSA_UNCOMPRESSED_KEY_BYTES: usize = 65;

/// Returns the error for an ECDSA public key that does not parse.
pub(crate) fn invalid_ecdsa_key(err: impl core::fmt::Display) -> CliError {
    CliError::InvalidArgument(format!("invalid ECDSA public key: {err}"))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use miden_client::account::standards::auth::NoAuth;
    use miden_client::account::{
        AccountBuilder,
        AccountBuilderSchemaCommitmentExt,
        AccountId,
        AccountType,
    };
    use miden_client::address::{Address, NetworkId};
    use miden_client::asset::AssetAmount;
    use miden_client::builder::ClientBuilder;
    use miden_client::keystore::FilesystemKeyStore;
    use miden_client::rpc::Endpoint;
    use miden_client::testing::account_id::ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET;
    use miden_client::testing::common::{create_test_store_path, fungible_faucet_component};
    use miden_client::{ClientError, ErrorHint, Serializable};
    use miden_client_sqlite_store::SqliteStore;

    use super::{
        FaucetMetadata,
        FaucetMetadataResolver,
        RawFaucetEntry,
        TokenParseError,
        base_units_to_tokens,
        faucet_metadata_from_bytes,
        faucet_metadata_to_bytes,
        tokens_to_base_units,
    };
    use crate::errors::CliError;

    fn amount(units: u64) -> AssetAmount {
        AssetAmount::new(units).unwrap()
    }

    #[test]
    fn convert_tokens_to_base_units() {
        assert_eq!(tokens_to_base_units("9223372.034707292160", 12).unwrap(), AssetAmount::MAX);
        assert_eq!(tokens_to_base_units("7531.2468", 8).unwrap(), amount(753_124_680_000));
        assert_eq!(tokens_to_base_units("7531.2468", 4).unwrap(), amount(75_312_468));
        assert_eq!(tokens_to_base_units("0", 3).unwrap(), AssetAmount::ZERO);
        assert_eq!(tokens_to_base_units("1234", 8).unwrap(), amount(123_400_000_000));
        assert_eq!(tokens_to_base_units("1", 0).unwrap(), amount(1));
        assert!(matches!(
            tokens_to_base_units("1.1", 0),
            Err(TokenParseError::TooManyDecimals(0))
        ),);
        assert!(matches!(
            tokens_to_base_units("18446744.073709551615", 11),
            Err(TokenParseError::TooManyDecimals(11))
        ),);
        assert!(matches!(tokens_to_base_units("123u3.23", 4), Err(TokenParseError::ParseU64(_))),);
        assert!(matches!(tokens_to_base_units("2.k3", 4), Err(TokenParseError::ParseU64(_))),);
        assert_eq!(tokens_to_base_units("12.345000", 4).unwrap(), amount(123_450));
        assert!(tokens_to_base_units("0.0001.00000001", 12).is_err());
        // Parses as a u64 but exceeds the maximum representable asset amount.
        assert!(matches!(
            tokens_to_base_units("18446744.073709551615", 12),
            Err(TokenParseError::InvalidAmount(_))
        ),);
    }

    #[test]
    fn convert_base_units_to_tokens() {
        assert_eq!(base_units_to_tokens(AssetAmount::MAX, 12), "9223372.034707292160");
        assert_eq!(base_units_to_tokens(amount(753_124_680_000), 8), "7531.24680000");
        assert_eq!(base_units_to_tokens(amount(75_312_468), 4), "7531.2468");
        assert_eq!(base_units_to_tokens(amount(75_312_468), 0), "75312468");
    }

    #[test]
    fn raw_faucet_entry_accepts_address_field() {
        let entries: BTreeMap<String, RawFaucetEntry> = toml::from_str(
            r#"BTC = { address = "mlcl1qru2e5yvx40ndgqqqzusrryr0ucyd0uj", decimals = 8 }"#,
        )
        .unwrap();

        assert_eq!(entries["BTC"].address, "mlcl1qru2e5yvx40ndgqqqzusrryr0ucyd0uj");
        assert_eq!(entries["BTC"].decimals, 8);
    }

    /// The token symbol map names faucets by address. An address of another network names a faucet
    /// on another chain, so the map must not load.
    #[test]
    fn faucet_metadata_resolver_rejects_address_from_another_network() {
        let faucet_id = AccountId::try_from(ACCOUNT_ID_PRIVATE_FUNGIBLE_FAUCET).unwrap();
        let address = Address::new(faucet_id).encode(NetworkId::Testnet);
        let path = std::env::temp_dir().join("token_symbol_map_network_mismatch.toml");
        std::fs::write(&path, format!(r#"BTC = {{ address = "{address}", decimals = 8 }}"#))
            .unwrap();

        let result = FaucetMetadataResolver::new(path.clone(), &NetworkId::Mainnet);
        std::fs::remove_file(&path).unwrap();

        let err = result.unwrap_err();
        let source = std::error::Error::source(&err).unwrap().to_string();
        assert!(
            source.contains("does not match configured network"),
            "unexpected error: {source}"
        );
    }

    #[tokio::test]
    async fn tracked_private_faucet_metadata_resolves_without_rpc() -> anyhow::Result<()> {
        let path = create_test_store_path();
        let endpoint = Endpoint::new("http".into(), "127.0.0.1".into(), Some(0));
        let resolver =
            FaucetMetadataResolver::new(path.with_extension("toml"), &endpoint.to_network_id())?;
        let mut client = ClientBuilder::<FilesystemKeyStore>::new()
            .grpc_client(&endpoint, Some(10))
            .store(Arc::new(SqliteStore::new(path).await?))
            .build()
            .await?;
        let (faucet_component, policies) = fungible_faucet_component()?;
        let faucet = AccountBuilder::new([42; 32])
            .account_type(AccountType::Private)
            .with_component(NoAuth)
            .with_component(faucet_component)
            .with_components(policies)
            .build_with_schema_commitment()?;
        client.add_account(&faucet, false).await?;

        let local = resolver
            .resolve_local(&client, faucet.id())
            .await?
            .expect("tracked faucet metadata");
        assert_eq!(local.symbol, "TEST");
        assert_eq!(local.decimals, 10);
        let metadata =
            resolver.resolve(&client, faucet.id()).await?.expect("offline faucet metadata");
        assert_eq!(metadata.symbol, local.symbol);
        assert_eq!(metadata.decimals, local.decimals);
        Ok(())
    }

    #[test]
    fn raw_faucet_entry_rejects_id_field() {
        let result = toml::from_str::<BTreeMap<String, RawFaucetEntry>>(
            r#"BTC = { id = "mlcl1qru2e5yvx40ndgqqqzusrryr0ucyd0uj", decimals = 8 }"#,
        );

        assert!(result.is_err());
    }

    #[test]
    fn the_kernel_rejection_of_an_empty_transaction_is_recognized() {
        assert!(super::is_empty_transaction_error(&super::empty_transaction_error()));
    }

    #[test]
    fn another_failed_assertion_is_not_an_empty_transaction() {
        let error = super::assertion_error("some other assertion");

        assert!(!super::is_empty_transaction_error(&error));
    }

    #[test]
    fn a_failed_program_gets_the_debug_adapter_hint() {
        let error = super::with_debug_adapter_hint(super::assertion_error("any assertion").into());

        let CliError::Client { help, .. } = error else {
            panic!("expected a client error, got {error:?}");
        };
        let expected = if cfg!(feature = "dap") {
            super::DEBUG_ADAPTER_HINT
        } else {
            super::DEBUG_ADAPTER_BUILD_HINT
        };
        assert_eq!(help.as_deref(), Some(expected));
    }

    #[test]
    fn another_client_error_keeps_its_hint() {
        let client_error = ClientError::AccountNonceTooLow;
        let library_help =
            Option::<ErrorHint>::from(&client_error).map(ErrorHint::into_help_message);

        let error = super::with_debug_adapter_hint(client_error.into());

        let CliError::Client { help, .. } = error else {
            panic!("expected a client error, got {error:?}");
        };
        assert!(library_help.is_some());
        assert_eq!(help, library_help);
    }

    #[test]
    fn faucet_metadata_round_trips_through_the_settings_encoding() {
        let meta = FaucetMetadata { symbol: "BTC".to_string(), decimals: 8 };

        let bytes = faucet_metadata_to_bytes(&meta);
        assert_eq!(bytes, ("BTC".to_string(), 8u8).to_bytes());
        assert_eq!(faucet_metadata_from_bytes(&bytes).unwrap(), meta);
    }

    #[test]
    fn faucet_metadata_decoding_rejects_a_truncated_value() {
        let meta = FaucetMetadata { symbol: "BTC".to_string(), decimals: 8 };
        let bytes = faucet_metadata_to_bytes(&meta);

        assert!(faucet_metadata_from_bytes(&bytes[..bytes.len() - 1]).is_err());
        assert!(faucet_metadata_from_bytes(&[]).is_err());
    }
}

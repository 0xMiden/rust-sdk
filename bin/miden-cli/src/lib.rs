use std::env;
use std::ffi::OsString;
use std::ops::{Deref, DerefMut};
use std::sync::Arc;

use clap::{Parser, Subcommand};
use comfy_table::{Attribute, Cell, ContentArrangement, Table, presets};
use errors::CliError;
use miden_client::account::AccountHeader;
use miden_client::builder::ClientBuilder;
use miden_client::keystore::{FilesystemKeyStore, Keystore};
use miden_client::note_transport::grpc::GrpcNoteTransportClient;
use miden_client::rpc::GrpcClient;
use miden_client::store::{NoteFilter as ClientNoteFilter, OutputNoteRecord};
use miden_client_sqlite_store::ClientBuilderSqliteExt;

mod commands;
use commands::account::AccountCmd;
use commands::call::CallCmd;
use commands::clear_config::ClearConfigCmd;
use commands::exec::ExecCmd;
use commands::export::ExportCmd;
use commands::import::ImportCmd;
use commands::info::InfoCmd;
use commands::init::InitCmd;
use commands::network_note_status::NetworkNoteStatusCmd;
use commands::new_account::{NewAccountCmd, NewWalletCmd};
use commands::new_transactions::{ConsumeNotesCmd, MintCmd, PswapCmd, SendCmd, SwapCmd};
use commands::notes::NotesCmd;
use commands::sync::SyncCmd;
use commands::tags::TagsCmd;
use commands::transactions::TransactionCmd;

use self::utils::config_file_exists;
use crate::commands::address::AddressCmd;

pub type CliKeyStore = FilesystemKeyStore;

/// A Client configured using the CLI's system user configuration.
///
/// This is a wrapper around `Client<CliKeyStore>` that provides convenient
/// initialization methods while maintaining full compatibility with the
/// underlying Client API through `Deref`.
///
/// # Examples
///
/// ```no_run
/// use miden_client_cli::transaction::TransactionRequestBuilder;
/// use miden_client_cli::{CliClient, DebugMode};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create a CLI-configured client
/// let mut client = CliClient::new(DebugMode::Disabled).await?;
///
/// // All Client methods work automatically via Deref
/// client.sync_state().await?;
///
/// // Build and submit transactions
/// let req = TransactionRequestBuilder::new()
///     // ... configure transaction
///     .build()?;
///
/// // client.submit_new_transaction(req, target_account_id)?;
/// # Ok(())
/// # }
/// ```
pub struct CliClient(miden_client::Client<CliKeyStore>);

impl CliClient {
    /// Creates a new `CliClient` instance from an existing `CliConfig`.
    ///
    ///
    /// **⚠️ WARNING: This method bypasses the standard CLI configuration discovery logic and should
    /// only be used in specific scenarios such as testing or when you have explicit control
    /// requirements.**
    ///
    /// ## When NOT to use this method
    ///
    /// - **DO NOT** use this method if you want your application to behave like the CLI tool
    /// - **DO NOT** use this for general-purpose client initialization
    /// - **DO NOT** use this if you expect automatic local/global config resolution
    ///
    /// ## When to use this method
    ///
    /// - **Testing**: When you need to test with a specific configuration
    /// - **Explicit Control**: When you must load config from a non-standard location
    /// - **Programmatic Config**: When you're constructing configuration programmatically
    ///
    /// ## Recommended Alternative
    ///
    /// For standard client initialization that matches CLI behavior, use:
    /// ```ignore
    /// CliClient::new(debug_mode).await?
    /// ```
    ///
    /// This method **does not** follow the CLI's configuration priority logic (local → global).
    /// Instead, it uses exactly the configuration provided, which may not be what you expect.
    ///
    /// # Arguments
    ///
    /// * `config` - The CLI configuration to use (bypasses standard config discovery)
    /// * `debug_mode` - The debug mode setting ([`DebugMode::Enabled`] or [`DebugMode::Disabled`])
    ///
    /// # Returns
    ///
    /// A configured [`CliClient`] instance.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`] if:
    /// - Keystore initialization fails
    /// - Client builder fails to construct the client
    /// - Note transport connection fails (if configured)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::PathBuf;
    ///
    /// use miden_client_cli::{CliClient, CliConfig, DebugMode};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // BEWARE: This bypasses standard config discovery!
    /// // Only use if you know what you're doing.
    /// let config = CliConfig::from_dir(&PathBuf::from("/path/to/.miden"))?;
    /// let client = CliClient::from_config(config, DebugMode::Disabled).await?;
    ///
    /// // Prefer this for standard CLI-like behavior:
    /// let client = CliClient::new(DebugMode::Disabled).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_config(
        config: CliConfig,
        debug_mode: miden_client::DebugMode,
    ) -> Result<Self, CliError> {
        // Create keystore
        let keystore =
            CliKeyStore::new(config.secret_keys_directory.clone()).map_err(CliError::KeyStore)?;

        // Build client with the provided configuration
        let rpc_client = Arc::new(
            GrpcClient::new(&config.rpc.endpoint.clone().into(), config.rpc.timeout_ms)
                .with_max_decoding_message_size(CLI_MAX_RESPONSE_SIZE_BYTES),
        );

        let mut builder = ClientBuilder::new()
            .sqlite_store(config.store_filepath.clone())
            .rpc(rpc_client)
            .authenticator(Arc::new(keystore))
            .in_debug_mode(debug_mode)
            .tx_discard_delta(Some(TX_DISCARD_DELTA));

        // Add optional max_block_number_delta
        if let Some(delta) = config.max_block_number_delta {
            builder = builder.max_block_number_delta(delta);
        }

        // Add optional note transport client
        if let Some(tl_config) = config.note_transport {
            let note_transport_client =
                GrpcNoteTransportClient::new(tl_config.endpoint.clone(), tl_config.timeout_ms);
            builder = builder.note_transport(Arc::new(note_transport_client));
        }

        // Build and return the wrapped client
        let client = builder.build().await.map_err(CliError::from)?;
        Ok(CliClient(client))
    }

    /// Creates a new `CliClient` instance configured using the system user configuration.
    ///
    /// # ✅ Recommended Constructor
    ///
    /// **This is the recommended way to create a `CliClient` instance.**
    ///
    /// This method implements the configuration logic used by the CLI tool, allowing external
    /// projects to create a Client instance with the same configuration. It searches for
    /// configuration files in the following order:
    ///
    /// 1. Local `.miden/miden-client.toml` in the current working directory
    /// 2. Global `.miden/miden-client.toml` in the home directory
    ///
    /// If no configuration file is found, it silently initializes a default configuration.
    ///
    /// The client is initialized with:
    /// - `SQLite` store from the configured path
    /// - `gRPC` client connection to the configured RPC endpoint
    /// - Filesystem-based keystore authenticator
    /// - Optional note transport client (if configured)
    /// - Transaction graceful blocks delta
    /// - Optional max block number delta
    ///
    /// # Arguments
    ///
    /// * `debug_mode` - The debug mode setting ([`DebugMode::Enabled`] or [`DebugMode::Disabled`]).
    ///
    /// # Returns
    ///
    /// A configured [`CliClient`] instance.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`] if:
    /// - No configuration file is found (local or global)
    /// - Configuration file parsing fails
    /// - Keystore initialization fails
    /// - Client builder fails to construct the client
    /// - Note transport connection fails (if configured)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use miden_client_cli::transaction::TransactionRequestBuilder;
    /// use miden_client_cli::{CliClient, DebugMode};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Create a client with default settings (debug disabled)
    /// let mut client = CliClient::new(DebugMode::Disabled).await?;
    ///
    /// // Or with debug mode enabled
    /// let mut client = CliClient::new(DebugMode::Enabled).await?;
    ///
    /// // Use it like a regular Client
    /// client.sync_state().await?;
    ///
    /// // Build and submit transactions
    /// let req = TransactionRequestBuilder::new()
    ///     // ... configure transaction
    ///     .build()?;
    ///
    /// // client.submit_new_transaction(req, target_account_id)?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn new(debug_mode: miden_client::DebugMode) -> Result<Self, CliError> {
        // Check if client is not yet initialized => silently initialize the client
        if !config_file_exists()? {
            let init_cmd = InitCmd::default();
            init_cmd.execute()?;
        }

        // Load configuration from system
        let config = CliConfig::load()?;

        // Create client using the loaded configuration
        Self::from_config(config, debug_mode).await
    }

    /// Unwraps the `CliClient` to get the inner `Client<CliKeyStore>`.
    ///
    /// This consumes the `CliClient` and returns the underlying client.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use miden_client_cli::{CliClient, DebugMode};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let cli_client = CliClient::new(DebugMode::Disabled).await?;
    /// let inner_client = cli_client.into_inner();
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_inner(self) -> miden_client::Client<CliKeyStore> {
        self.0
    }
}

/// Allows using `CliClient` like `Client<CliKeyStore>` through deref coercion.
///
/// This enables calling all `Client` methods on `CliClient` directly.
impl Deref for CliClient {
    type Target = miden_client::Client<CliKeyStore>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// Allows mutable access to `Client<CliKeyStore>` methods.
impl DerefMut for CliClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

mod advice_inputs;
pub mod config;
// These modules intentionally shadow the miden_client re-exports - CLI has its own errors/utils
#[allow(hidden_glob_reexports)]
mod errors;
mod info;
#[allow(hidden_glob_reexports)]
mod utils;

/// Re-export `MIDEN_DIR` for use in tests
pub use config::MIDEN_DIR;
/// Re-export common types for external projects
pub use config::{CLIENT_CONFIG_FILE_NAME, CliConfig};
pub use errors::CliError as Error;
/// Re-export the entire `miden_client` crate so external projects can use a single dependency.
pub use miden_client::*;

/// Client binary name.
///
/// If, for whatever reason, we fail to obtain the client's executable name,
/// then we simply display the standard "miden-client".
pub fn client_binary_name() -> OsString {
    std::env::current_exe()
        .inspect_err(|e| {
            eprintln!(
                "WARNING: Couldn't obtain the path of the current executable because of {e}.\
             Defaulting to miden-client."
            );
        })
        .and_then(|executable_path| {
            executable_path.file_name().map(std::ffi::OsStr::to_os_string).ok_or(
                std::io::Error::other("Couldn't obtain the file name of the current executable"),
            )
        })
        .unwrap_or(OsString::from("miden-client"))
}

/// Number of blocks that must elapse after a transaction’s reference block before it is marked
/// stale and discarded.
const TX_DISCARD_DELTA: u32 = 20;

/// Maximum size (in bytes) of any decoded gRPC response the CLI accepts. Sized to fit large
/// `SyncTransactions` responses.
const CLI_MAX_RESPONSE_SIZE_BYTES: usize = 6 * 1024 * 1024;

/// Root CLI struct.
#[derive(Parser, Debug)]
#[command(
    name = "miden-client",
    about = "The Miden client",
    version,
    propagate_version = true,
    rename_all = "kebab-case"
)]
#[command(multicall(true))]
pub struct MidenClientCli {
    #[command(subcommand)]
    behavior: Behavior,
}

impl From<MidenClientCli> for Cli {
    fn from(value: MidenClientCli) -> Self {
        match value.behavior {
            Behavior::MidenClient { cli } => cli,
            Behavior::External(args) => Cli::parse_from(args).set_external(),
        }
    }
}

#[derive(Debug, Subcommand)]
#[command(rename_all = "kebab-case")]
enum Behavior {
    /// The Miden Client CLI.
    MidenClient {
        #[command(flatten)]
        cli: Cli,
    },

    /// Used when the Miden Client CLI is called under a different name, like
    /// when it is called from [Midenup](https://github.com/0xMiden/midenup).
    /// Vec<OsString> holds the "raw" arguments passed to the command line,
    /// analogous to `argv`.
    #[command(external_subcommand)]
    External(Vec<OsString>),
}

#[derive(Parser, Debug)]
#[command(name = "miden-client")]
pub struct Cli {
    /// Activates the executor's debug mode, which enables debug output for scripts
    /// that were compiled and executed with this mode.
    #[arg(short, long, default_value_t = false)]
    debug: bool,

    #[command(subcommand)]
    action: Command,

    /// Indicates whether the client's CLI is being called directly, or
    /// externally under an alias (like in the case of
    /// [Midenup](https://github.com/0xMiden/midenup).
    #[arg(skip)]
    #[allow(unused)]
    external: bool,
}

/// CLI actions.
#[derive(Debug, Parser)]
pub enum Command {
    Account(AccountCmd),
    NewAccount(NewAccountCmd),
    NewWallet(NewWalletCmd),
    Import(ImportCmd),
    Export(ExportCmd),
    Init(InitCmd),
    ClearConfig(ClearConfigCmd),
    Notes(NotesCmd),
    Sync(SyncCmd),
    /// View a summary of the current client state.
    Info(InfoCmd),
    Tags(TagsCmd),
    Address(AddressCmd),
    #[command(name = "tx")]
    Transaction(TransactionCmd),
    Mint(MintCmd),
    Send(SendCmd),
    Pswap(PswapCmd),
    Swap(SwapCmd),
    ConsumeNotes(ConsumeNotesCmd),
    Exec(ExecCmd),
    NetworkNoteStatus(NetworkNoteStatusCmd),
    Call(CallCmd),
}

/// CLI entry point.
impl Cli {
    pub async fn execute(&self) -> Result<(), CliError> {
        // Handle commands that don't require client initialization
        match &self.action {
            Command::Init(init_cmd) => {
                init_cmd.execute()?;
                return Ok(());
            },
            Command::ClearConfig(clear_config_cmd) => {
                clear_config_cmd.execute()?;
                return Ok(());
            },
            Command::NetworkNoteStatus(cmd) => {
                return cmd.execute().await;
            },
            _ => {},
        }

        // Check if Client is not yet initialized => silently initialize the client
        if !config_file_exists()? {
            let init_cmd = InitCmd::default();
            init_cmd.execute()?;
        }

        // Define whether we want to use the executor's debug mode based on the env var and
        // the flag override
        let in_debug_mode = match env::var("MIDEN_DEBUG") {
            Ok(value) if value.to_lowercase() == "true" => miden_client::DebugMode::Enabled,
            _ => miden_client::DebugMode::Disabled,
        };

        // Load configuration
        let cli_config = CliConfig::load()?;

        // Create keystore for commands that need it
        let keystore = CliKeyStore::new(cli_config.secret_keys_directory.clone())
            .map_err(CliError::KeyStore)?;

        // Create the client
        let cli_client = CliClient::from_config(cli_config, in_debug_mode).await?;

        // Extract the inner client for command execution
        let client = cli_client.into_inner();

        // Execute CLI command
        match &self.action {
            Command::Account(account) => account.execute(client).await,
            Command::NewWallet(new_wallet) => Box::pin(new_wallet.execute(client, keystore)).await,
            Command::NewAccount(new_account) => {
                Box::pin(new_account.execute(client, keystore)).await
            },
            Command::Import(import) => import.execute(client, keystore).await,
            Command::Init(_) | Command::ClearConfig(_) | Command::NetworkNoteStatus(_) => Ok(()), /* Already handled earlier */
            Command::Info(info_cmd) => info::print_client_info(&client, info_cmd.rpc_status).await,
            Command::Notes(notes) => Box::pin(notes.execute(client)).await,
            Command::Sync(sync) => sync.execute(client).await,
            Command::Tags(tags) => tags.execute(client).await,
            Command::Address(addresses) => addresses.execute(client).await,
            Command::Transaction(transaction) => transaction.execute(client).await,
            Command::Exec(execute_program) => Box::pin(execute_program.execute(client)).await,
            Command::Call(call) => Box::pin(call.execute(client)).await,
            Command::Export(cmd) => cmd.execute(client, keystore).await,
            Command::Mint(mint) => Box::pin(mint.execute(client)).await,
            Command::Send(send) => Box::pin(send.execute(client)).await,
            Command::Pswap(pswap) => Box::pin(pswap.execute(client)).await,
            Command::Swap(swap) => Box::pin(swap.execute(client)).await,
            Command::ConsumeNotes(consume_notes) => Box::pin(consume_notes.execute(client)).await,
        }
    }

    fn set_external(mut self) -> Self {
        self.external = true;
        self
    }
}

pub fn create_dynamic_table(headers: &[&str]) -> Table {
    let header_cells = headers
        .iter()
        .map(|header| Cell::new(header).add_attribute(Attribute::Bold))
        .collect::<Vec<_>>();

    let mut table = Table::new();
    table
        .load_preset(presets::UTF8_FULL)
        .set_content_arrangement(ContentArrangement::DynamicFullWidth)
        .set_header(header_cells);

    table
}

/// Returns the client output note whose ID starts with `note_id_prefix`.
///
/// # Errors
///
/// - Returns [`IdPrefixFetchError::NoMatch`](miden_client::IdPrefixFetchError::NoMatch) if we were
///   unable to find any note where `note_id_prefix` is a prefix of its ID.
/// - Returns [`IdPrefixFetchError::MultipleMatches`](miden_client::IdPrefixFetchError::MultipleMatches)
///   if there were more than one note found where `note_id_prefix` is a prefix of its ID.
pub(crate) async fn get_output_note_with_id_prefix<AUTH: Keystore + Sync>(
    client: &miden_client::Client<AUTH>,
    note_id_prefix: &str,
) -> Result<OutputNoteRecord, miden_client::IdPrefixFetchError> {
    let mut output_note_records = client
        .get_output_notes(ClientNoteFilter::All)
        .await
        .map_err(|err| {
            tracing::error!("Error when fetching all notes from the store: {err}");
            miden_client::IdPrefixFetchError::NoMatch(
                format!("note ID prefix {note_id_prefix}").to_string(),
            )
        })?
        .into_iter()
        .filter(|note_record| note_record.id().to_hex().starts_with(note_id_prefix))
        .collect::<Vec<_>>();

    if output_note_records.is_empty() {
        return Err(miden_client::IdPrefixFetchError::NoMatch(
            format!("note ID prefix {note_id_prefix}").to_string(),
        ));
    }
    if output_note_records.len() > 1 {
        let output_note_record_ids =
            output_note_records.iter().map(OutputNoteRecord::id).collect::<Vec<_>>();
        tracing::error!(
            "Multiple notes found for the prefix {}: {:?}",
            note_id_prefix,
            output_note_record_ids
        );
        return Err(miden_client::IdPrefixFetchError::MultipleMatches(
            format!("note ID prefix {note_id_prefix}").to_string(),
        ));
    }

    Ok(output_note_records
        .pop()
        .expect("input_note_records should always have one element"))
}

/// Returns the client account whose ID starts with `account_id_prefix`.
///
/// # Errors
///
/// - Returns [`IdPrefixFetchError::NoMatch`](miden_client::IdPrefixFetchError::NoMatch) if we were
///   unable to find any account where `account_id_prefix` is a prefix of its ID.
/// - Returns [`IdPrefixFetchError::MultipleMatches`](miden_client::IdPrefixFetchError::MultipleMatches)
///   if there were more than one account found where `account_id_prefix` is a prefix of its ID.
async fn get_account_with_id_prefix<AUTH>(
    client: &miden_client::Client<AUTH>,
    account_id_prefix: &str,
) -> Result<AccountHeader, miden_client::IdPrefixFetchError> {
    let mut accounts = client
        .get_account_headers()
        .await
        .map_err(|err| {
            tracing::error!("Error when fetching all accounts from the store: {err}");
            miden_client::IdPrefixFetchError::NoMatch(
                format!("account ID prefix {account_id_prefix}").to_string(),
            )
        })?
        .into_iter()
        .filter(|(account_header, _)| account_header.id().to_hex().starts_with(account_id_prefix))
        .map(|(acc, _)| acc)
        .collect::<Vec<_>>();

    if accounts.is_empty() {
        return Err(miden_client::IdPrefixFetchError::NoMatch(
            format!("account ID prefix {account_id_prefix}").to_string(),
        ));
    }
    if accounts.len() > 1 {
        let account_ids = accounts.iter().map(AccountHeader::id).collect::<Vec<_>>();
        tracing::error!(
            "Multiple accounts found for the prefix {}: {:?}",
            account_id_prefix,
            account_ids
        );
        return Err(miden_client::IdPrefixFetchError::MultipleMatches(
            format!("account ID prefix {account_id_prefix}").to_string(),
        ));
    }

    Ok(accounts.pop().expect("account_ids should always have one element"))
}

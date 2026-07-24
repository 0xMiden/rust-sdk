use core::fmt::Debug;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use figment::providers::{Format, Toml};
use figment::value::{Dict, Map};
use figment::{Figment, Metadata, Profile, Provider};
use miden_client::note_transport::{
    NOTE_TRANSPORT_DEVNET_ENDPOINT,
    NOTE_TRANSPORT_TESTNET_ENDPOINT,
};
use miden_client::rpc::Endpoint;
use serde::{Deserialize, Serialize};

use crate::errors::CliError;

pub const MIDEN_DIR: &str = ".miden";
pub const CLIENT_CONFIG_FILE_NAME: &str = "miden-client.toml";
pub const TOKEN_SYMBOL_MAP_FILENAME: &str = "token_symbol_map.toml";
pub const DEFAULT_PACKAGES_DIR: &str = "packages";
pub const STORE_FILENAME: &str = "store.sqlite3";
pub const KEYSTORE_DIRECTORY: &str = "keystore";
pub const DEFAULT_REMOTE_PROVER_TIMEOUT: Duration = Duration::from_secs(20);

/// Returns the global miden directory path.
///
/// If the `MIDEN_CLIENT_HOME` environment variable is set, returns that path directly.
/// Otherwise, returns the `.miden` directory in the user's home directory.
pub fn get_global_miden_dir() -> Result<PathBuf, std::io::Error> {
    if let Ok(miden_home) = std::env::var("MIDEN_CLIENT_HOME") {
        return Ok(PathBuf::from(miden_home));
    }
    dirs::home_dir()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "Could not determine home directory")
        })
        .map(|home| home.join(MIDEN_DIR))
}

/// Returns the local miden directory path relative to the current working directory
pub fn get_local_miden_dir() -> Result<PathBuf, std::io::Error> {
    std::env::current_dir().map(|cwd| cwd.join(MIDEN_DIR))
}

// CLI CONFIG
// ================================================================================================

/// Whether the configuration was loaded from the local or global `.miden` directory.
#[derive(Debug, Clone)]
pub enum ConfigKind {
    Local,
    Global,
}

/// The `.miden` directory from which the configuration was loaded.
#[derive(Debug, Clone)]
pub struct ConfigDir {
    pub path: PathBuf,
    pub kind: ConfigKind,
}

impl std::fmt::Display for ConfigDir {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({:?})", self.path.display(), self.kind)
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct CliConfig {
    /// The directory this configuration was loaded from. Not part of the TOML file.
    #[serde(skip)]
    pub config_dir: Option<ConfigDir>,
    /// Describes settings related to the RPC endpoint.
    pub rpc: RpcConfig,
    /// Path to the `SQLite` store file.
    pub store_filepath: PathBuf,
    /// Path to the directory that contains the secret key files.
    pub secret_keys_directory: PathBuf,
    /// Path to the file containing the token symbol map.
    pub token_symbol_map_filepath: PathBuf,
    /// RPC endpoint for the remote prover. If this isn't present, a local prover will be used.
    pub remote_prover_endpoint: Option<CliEndpoint>,
    /// Path to the directory from where packages will be loaded.
    pub package_directory: PathBuf,
    /// Maximum number of blocks the client can be behind the network for transactions and account
    /// proofs to be considered valid.
    pub max_block_number_delta: Option<u32>,
    /// Describes settings related to the note transport endpoint.
    pub note_transport: Option<NoteTransportConfig>,
    /// Timeout for the remote prover requests.
    pub remote_prover_timeout: Duration,
}

// Make `ClientConfig` a provider itself for composability.
impl Provider for CliConfig {
    fn metadata(&self) -> Metadata {
        Metadata::named("CLI Config")
    }

    fn data(&self) -> Result<Map<Profile, Dict>, figment::Error> {
        figment::providers::Serialized::defaults(CliConfig::default()).data()
    }

    fn profile(&self) -> Option<Profile> {
        // Optionally, a profile that's selected by default.
        None
    }
}

/// Default implementation for `CliConfig`.
///
/// **Note**: This implementation is primarily used by the [`figment`] `Provider` trait
/// (see [`CliConfig::data()`]) to provide default values during configuration merging.
/// The paths returned are relative and intended to be resolved against a `.miden` directory.
///
/// For loading configuration from the filesystem, use [`CliConfig::load()`] instead.
impl Default for CliConfig {
    fn default() -> Self {
        // Create paths relative to the config file location (which is in .miden directory)
        // These will be resolved relative to the .miden directory when the config is loaded
        Self {
            config_dir: None,
            rpc: RpcConfig::default(),
            store_filepath: PathBuf::from(STORE_FILENAME),
            secret_keys_directory: PathBuf::from(KEYSTORE_DIRECTORY),
            token_symbol_map_filepath: PathBuf::from(TOKEN_SYMBOL_MAP_FILENAME),
            remote_prover_endpoint: None,
            package_directory: PathBuf::from(DEFAULT_PACKAGES_DIR),
            max_block_number_delta: None,
            note_transport: None,
            remote_prover_timeout: DEFAULT_REMOTE_PROVER_TIMEOUT,
        }
    }
}

impl CliConfig {
    /// Returns `true` when this config was loaded from the local `.miden` directory.
    ///
    /// This is typically set when loading via [`CliConfig::from_local_dir`] or
    /// [`CliConfig::load`] (when local takes precedence).
    pub fn is_local(&self) -> bool {
        matches!(&self.config_dir, Some(ConfigDir { kind: ConfigKind::Local, .. }))
    }

    /// Returns `true` when this config was loaded from the global `.miden` directory.
    ///
    /// This is typically set when loading via [`CliConfig::from_global_dir`] or
    /// [`CliConfig::load`] (when local config is not available).
    pub fn is_global(&self) -> bool {
        matches!(&self.config_dir, Some(ConfigDir { kind: ConfigKind::Global, .. }))
    }

    /// Loads configuration from a specific `.miden` directory.
    ///
    /// # ⚠️ WARNING: Advanced Use Only
    ///
    /// **This method bypasses the standard CLI configuration discovery logic.**
    ///
    /// This method loads config from an explicitly specified directory, which means:
    /// - It does NOT check for local `.miden` directory first
    /// - It does NOT fall back to global `~/.miden` directory
    /// - It does NOT follow CLI priority logic
    ///
    /// ## Recommended Alternative
    ///
    /// For standard CLI-like configuration loading, use:
    /// ```ignore
    /// CliConfig::load()  // Respects local → global priority
    /// ```
    ///
    /// Or for client initialization:
    /// ```ignore
    /// CliClient::new().await?
    /// ```
    ///
    /// ## When to use this method
    ///
    /// - **Testing**: When you need to test with config from a specific directory
    /// - **Explicit Control**: When you must load from a non-standard location
    ///
    /// # Arguments
    ///
    /// * `miden_dir` - Path to the `.miden` directory containing `miden-client.toml`
    ///
    /// # Returns
    ///
    /// A configured [`CliConfig`] instance with resolved paths.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`](crate::errors::CliError):
    /// - [`CliError::ConfigNotFound`](crate::errors::CliError::ConfigNotFound) if the config file
    ///   doesn't exist in the specified directory
    /// - [`CliError::Config`](crate::errors::CliError::Config) if configuration file parsing fails
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::path::PathBuf;
    ///
    /// use miden_client_cli::config::CliConfig;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // ⚠️ This bypasses standard config discovery!
    /// let config = CliConfig::from_dir(&PathBuf::from("/path/to/.miden"))?;
    ///
    /// // ✅ Prefer this for CLI-like behavior:
    /// let config = CliConfig::load()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_dir(miden_dir: &Path) -> Result<Self, CliError> {
        let config_path = miden_dir.join(CLIENT_CONFIG_FILE_NAME);

        if !config_path.exists() {
            return Err(CliError::ConfigNotFound(format!(
                "Config file does not exist at {}",
                config_path.display()
            )));
        }

        let mut cli_config = Self::load_from_file(&config_path)?;

        // Resolve all relative paths relative to the .miden directory
        Self::resolve_relative_path(&mut cli_config.store_filepath, miden_dir);
        Self::resolve_relative_path(&mut cli_config.secret_keys_directory, miden_dir);
        Self::resolve_relative_path(&mut cli_config.token_symbol_map_filepath, miden_dir);
        Self::resolve_relative_path(&mut cli_config.package_directory, miden_dir);

        Ok(cli_config)
    }

    /// Loads configuration from the local `.miden` directory (current working directory).
    ///
    /// # ⚠️ WARNING: Advanced Use Only
    ///
    /// **This method bypasses the standard CLI configuration discovery logic.**
    ///
    /// This method ONLY checks the local directory and does NOT fall back to the global
    /// configuration if the local config doesn't exist. This differs from CLI behavior.
    ///
    /// ## Recommended Alternative
    ///
    /// For standard CLI-like behavior:
    /// ```ignore
    /// CliConfig::load()  // Respects local → global fallback
    /// CliClient::new().await?
    /// ```
    ///
    /// ## When to use this method
    ///
    /// - **Testing**: When you need to ensure only local config is used
    /// - **Explicit Control**: When you must avoid global config
    ///
    /// # Returns
    ///
    /// A configured [`CliConfig`] instance.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`](crate::errors::CliError) if:
    /// - Cannot determine current working directory
    /// - The config file doesn't exist locally
    /// - Configuration file parsing fails
    pub fn from_local_dir() -> Result<Self, CliError> {
        let local_miden_dir = get_local_miden_dir()?;
        let mut config = Self::from_dir(&local_miden_dir)?;
        config.config_dir = Some(ConfigDir {
            path: local_miden_dir,
            kind: ConfigKind::Local,
        });
        Ok(config)
    }

    /// Loads configuration from the global `.miden` directory (user's home directory).
    ///
    /// # ⚠️ WARNING: Advanced Use Only
    ///
    /// **This method bypasses the standard CLI configuration discovery logic.**
    ///
    /// This method ONLY checks the global directory and does NOT check for local config first.
    /// This differs from CLI behavior which prioritizes local config over global.
    ///
    /// ## Recommended Alternative
    ///
    /// For standard CLI-like behavior:
    /// ```ignore
    /// CliConfig::load()  // Respects local → global priority
    /// CliClient::new().await?
    /// ```
    ///
    /// ## When to use this method
    ///
    /// - **Testing**: When you need to ensure only global config is used
    /// - **Explicit Control**: When you must bypass local config
    ///
    /// # Returns
    ///
    /// A configured [`CliConfig`] instance.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`](crate::errors::CliError) if:
    /// - Cannot determine home directory
    /// - The config file doesn't exist globally
    /// - Configuration file parsing fails
    pub fn from_global_dir() -> Result<Self, CliError> {
        let global_miden_dir = get_global_miden_dir().map_err(|e| {
            CliError::Config(Box::new(e), "Failed to determine global config directory".to_string())
        })?;
        let mut config = Self::from_dir(&global_miden_dir)?;
        config.config_dir = Some(ConfigDir {
            path: global_miden_dir,
            kind: ConfigKind::Global,
        });
        Ok(config)
    }

    /// Loads configuration from system directories with priority: local first, then global
    /// fallback.
    ///
    /// # ✅ Recommended Method
    ///
    /// **This is the recommended method for loading CLI configuration as it follows the same
    /// discovery logic as the CLI tool itself.**
    ///
    /// This method searches for configuration files in the following order:
    /// 1. Local `.miden/miden-client.toml` in the current working directory
    /// 2. Global `.miden/miden-client.toml` in the home directory (fallback)
    ///
    /// This matches the CLI's configuration priority logic. For most use cases, you should
    /// use [`CliClient::new()`](crate::CliClient::new) instead, which uses this method
    /// internally.
    ///
    /// # Returns
    ///
    /// A configured [`CliConfig`] instance.
    ///
    /// # Errors
    ///
    /// Returns a [`CliError`](crate::errors::CliError):
    /// - [`CliError::ConfigNotFound`](crate::errors::CliError::ConfigNotFound) if neither local nor
    ///   global config file exists
    /// - [`CliError::Config`](crate::errors::CliError::Config) if configuration file parsing fails
    ///
    /// Note: If a local config file exists but has parse errors, the error is returned
    /// immediately without falling back to global config.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use miden_client_cli::config::CliConfig;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // ✅ Recommended: Loads from local .miden dir if it exists, otherwise from global
    /// let config = CliConfig::load()?;
    ///
    /// // Or even better, use CliClient directly:
    /// // let client = CliClient::new().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn load() -> Result<Self, CliError> {
        // Try local first
        match Self::from_local_dir() {
            Ok(config) => Ok(config),
            // Only fall back to global if the local config file was not found
            // (not for parse errors or other issues)
            Err(CliError::ConfigNotFound(_)) => {
                // Fall back to global
                Self::from_global_dir().map_err(|e| match e {
                    CliError::ConfigNotFound(_) => CliError::ConfigNotFound(
                        "Neither local nor global config file exists".to_string(),
                    ),
                    other => other,
                })
            },
            // For other errors (like parse errors), propagate them immediately
            Err(e) => Err(e),
        }
    }

    /// Loads the client configuration from a TOML file.
    fn load_from_file(config_file: &Path) -> Result<Self, CliError> {
        Figment::from(Toml::file(config_file)).extract().map_err(|err| {
            CliError::Config("failed to load config file".to_string().into(), err.to_string())
        })
    }

    /// Resolves a relative path against a base directory.
    /// If the path is already absolute, it remains unchanged.
    fn resolve_relative_path(path: &mut PathBuf, base_dir: &Path) {
        if path.is_relative() {
            *path = base_dir.join(&*path);
        }
    }
}

// RPC CONFIG
// ================================================================================================

/// Settings for the RPC client.
#[derive(Debug, Deserialize, Serialize)]
pub struct RpcConfig {
    /// Address of the Miden node to connect to.
    pub endpoint: CliEndpoint,
    /// Timeout for the RPC api requests, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for RpcConfig {
    fn default() -> Self {
        Self {
            endpoint: Endpoint::testnet().into(),
            timeout_ms: 10000,
        }
    }
}

// NOTE TRANSPORT CONFIG
// ================================================================================================

/// Settings for the note transport client.
#[derive(Debug, Deserialize, Serialize)]
pub struct NoteTransportConfig {
    /// Address of the Miden Note Transport node to connect to.
    pub endpoint: String,
    /// Timeout for the Note Transport RPC api requests, in milliseconds.
    pub timeout_ms: u64,
}

impl Default for NoteTransportConfig {
    fn default() -> Self {
        Self {
            endpoint: NOTE_TRANSPORT_TESTNET_ENDPOINT.to_string(),
            timeout_ms: 10000,
        }
    }
}

impl NoteTransportConfig {
    /// Returns a `NoteTransportConfig` for the devnet network.
    pub fn devnet() -> Self {
        Self {
            endpoint: NOTE_TRANSPORT_DEVNET_ENDPOINT.to_string(),
            timeout_ms: 10000,
        }
    }
}

// CLI ENDPOINT
// ================================================================================================

#[derive(Clone, Debug)]
pub struct CliEndpoint(pub Endpoint);

impl Display for CliEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl TryFrom<&str> for CliEndpoint {
    type Error = String;

    fn try_from(endpoint: &str) -> Result<Self, Self::Error> {
        let endpoint = Endpoint::try_from(endpoint).map_err(|err| err.clone())?;
        Ok(Self(endpoint))
    }
}

impl From<Endpoint> for CliEndpoint {
    fn from(endpoint: Endpoint) -> Self {
        Self(endpoint)
    }
}

impl TryFrom<Network> for CliEndpoint {
    type Error = CliError;

    fn try_from(value: Network) -> Result<Self, Self::Error> {
        Ok(Self(Endpoint::try_from(value.to_rpc_endpoint().as_str()).map_err(|err| {
            CliError::Parse(err.into(), "Failed to parse RPC endpoint".to_string())
        })?))
    }
}

impl From<CliEndpoint> for Endpoint {
    fn from(endpoint: CliEndpoint) -> Self {
        endpoint.0
    }
}

impl From<&CliEndpoint> for Endpoint {
    fn from(endpoint: &CliEndpoint) -> Self {
        endpoint.0.clone()
    }
}

impl Serialize for CliEndpoint {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for CliEndpoint {
    fn deserialize<D>(deserializer: D) -> Result<CliEndpoint, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let endpoint = String::deserialize(deserializer)?;
        CliEndpoint::try_from(endpoint.as_str()).map_err(serde::de::Error::custom)
    }
}

// NETWORK
// ================================================================================================

/// Represents the network to which the client connects. It is used to determine the RPC endpoint
/// and network ID for the CLI.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum Network {
    Custom(String),
    Devnet,
    Localhost,
    Testnet,
}

impl FromStr for Network {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "devnet" => Ok(Network::Devnet),
            "localhost" => Ok(Network::Localhost),
            "testnet" => Ok(Network::Testnet),
            custom => Ok(Network::Custom(custom.to_string())),
        }
    }
}

impl Network {
    /// Converts the Network variant to its corresponding RPC endpoint string
    #[allow(dead_code)]
    pub fn to_rpc_endpoint(&self) -> String {
        match self {
            Network::Custom(custom) => custom.clone(),
            Network::Devnet => Endpoint::devnet().to_string(),
            Network::Localhost => Endpoint::default().to_string(),
            Network::Testnet => Endpoint::testnet().to_string(),
        }
    }
}

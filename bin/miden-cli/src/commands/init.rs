use std::fs::{self, File};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use tracing::info;

use crate::CLIENT_CONFIG_FILE_NAME;
use crate::config::{
    CliConfig,
    CliEndpoint,
    MIDEN_DIR,
    Network,
    NoteTransportConfig,
    get_global_miden_dir,
    get_local_miden_dir,
};
use crate::errors::CliError;

// COMPONENT PACKAGES
// ================================================================================================

/// Contains the account component template file generated on build.rs, corresponding to the basic
/// wallet component.
const BASIC_WALLET_PACKAGE: (&str, &[u8]) = (
    "basic-wallet.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/", "basic-wallet.masp")),
);

/// Contains the account component template file generated on build.rs, corresponding to the
/// fungible faucet component.
const FAUCET_PACKAGE: (&str, &[u8]) = (
    "basic-fungible-faucet.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/", "basic-fungible-faucet.masp")),
);

/// Contains the account component template file generated on build.rs, corresponding to the
/// non-fungible faucet component.
const NON_FUNGIBLE_FAUCET_PACKAGE: (&str, &[u8]) = (
    "basic-non-fungible-faucet.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/", "basic-non-fungible-faucet.masp")),
);

// AUTH COMPONENT PACKAGES
// ================================================================================================

/// Contains the account component template file generated on build.rs, corresponding to the basic
/// auth component.
const BASIC_AUTH_PACKAGE: (&str, &[u8]) = (
    "auth/basic-auth.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/auth/", "basic-auth.masp")),
);

const ECDSA_AUTH_PACKAGE: (&str, &[u8]) = (
    "auth/ecdsa-auth.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/auth/", "ecdsa-auth.masp")),
);

const NO_AUTH_PACKAGE: (&str, &[u8]) = (
    "auth/no-auth.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/auth/", "no-auth.masp")),
);

const MULTISIG_AUTH_PACKAGE: (&str, &[u8]) = (
    "auth/multisig-auth.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/auth/", "multisig-auth.masp")),
);

const GUARDED_MULTISIG_AUTH_PACKAGE: (&str, &[u8]) = (
    "auth/guarded-multisig-auth.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/auth/", "guarded-multisig-auth.masp")),
);

const NETWORK_ACCOUNT_AUTH_PACKAGE: (&str, &[u8]) = (
    "auth/network-account-auth.masp",
    include_bytes!(concat!(env!("OUT_DIR"), "/packages/auth/", "network-account-auth.masp")),
);

const DEFAULT_INCLUDED_PACKAGES: [(&str, &[u8]); 9] = [
    BASIC_WALLET_PACKAGE,
    FAUCET_PACKAGE,
    NON_FUNGIBLE_FAUCET_PACKAGE,
    BASIC_AUTH_PACKAGE,
    ECDSA_AUTH_PACKAGE,
    NO_AUTH_PACKAGE,
    MULTISIG_AUTH_PACKAGE,
    GUARDED_MULTISIG_AUTH_PACKAGE,
    NETWORK_ACCOUNT_AUTH_PACKAGE,
];

// INIT COMMAND
// ================================================================================================

#[derive(Debug, Clone, Parser, Default)]
#[command(
    about = "Initialize the client. By default creates a global `.miden` directory in the home directory. \
Use --local to create a local `.miden` directory in the current working directory."
)]
pub struct InitCmd {
    /// Create configuration in the local working directory instead of the global home directory
    #[clap(long)]
    local: bool,

    /// Network configuration to use. Options are `devnet`, `testnet`, `localhost` or a custom RPC
    /// endpoint. By default, the command uses the Testnet network.
    #[clap(long, short)]
    network: Option<Network>,

    /// Path to the store file.
    #[arg(long)]
    store_path: Option<String>,

    /// RPC endpoint for the remote prover. Required if proving mode is set to remote.
    /// The endpoint must be in the form of "{protocol}://{hostname}:{port}", being the protocol
    /// and port optional.
    /// If the proving RPC isn't set, the proving mode will be set to local.
    #[arg(long)]
    remote_prover_endpoint: Option<String>,

    /// Timeout for the remote prover requests, in milliseconds.
    #[arg(long)]
    remote_prover_timeout_ms: Option<u64>,

    /// RPC endpoint for the note transport node. Required to use the note transport network to
    /// exchange private notes.
    /// The endpoint must be in the form of "{protocol}://{hostname}:{port}", being the protocol
    /// and port optional.
    #[arg(long)]
    note_transport_endpoint: Option<String>,

    /// Maximum number of blocks the client can be behind the network.
    #[clap(long)]
    block_delta: Option<u32>,
}

impl InitCmd {
    pub fn execute(&self) -> Result<(), CliError> {
        // Determine target directory based on flags
        let (target_miden_dir, config_type) = if self.local {
            (get_local_miden_dir()?, "local")
        } else {
            (
                get_global_miden_dir().map_err(|e| {
                    CliError::Config(Box::new(e), "Failed to determine home directory".to_string())
                })?,
                "global",
            )
        };

        let config_file_path = target_miden_dir.join(CLIENT_CONFIG_FILE_NAME);

        // Check if config already exists
        if config_file_path.exists() {
            // An unparsable file must still hint at `clear-config`, not `init`.
            let network_info = match CliConfig::from_dir(&target_miden_dir) {
                Ok(config) => {
                    format!(", configured with the network \"{}\"", config.rpc.endpoint)
                },
                Err(_) => ", but it could not be parsed".to_string(),
            };
            let global_flag: String = if self.local { "" } else { " --global" }.into();
            let error_msg = format!(
                "\"{}\" already exists in the {} {} directory ({}){}.",
                CLIENT_CONFIG_FILE_NAME,
                config_type,
                MIDEN_DIR,
                target_miden_dir.display(),
                network_info
            );
            return Err(CliError::ConfigAlreadyExists(error_msg, global_flag));
        }

        // Create the miden directory if not existent
        fs::create_dir_all(&target_miden_dir).map_err(|err| {
            CliError::Config(
                Box::new(err),
                format!(
                    "failed to create {} {} directory in {}",
                    config_type,
                    MIDEN_DIR,
                    target_miden_dir.display()
                ),
            )
        })?;

        // Create new config for target directory
        let mut cli_config = CliConfig::default();

        if let Some(network) = &self.network {
            cli_config.rpc.endpoint = CliEndpoint::try_from(network.clone())?;
        }

        if let Some(path) = &self.store_path {
            cli_config.store_filepath = PathBuf::from(path);
        }

        cli_config.remote_prover_endpoint = match &self.remote_prover_endpoint {
            Some(rpc) => Some(CliEndpoint::try_from(rpc.as_str()).map_err(|err| {
                CliError::Parse(err.into(), "Failed to parse remote prover endpoint".to_string())
            })?),
            None => None,
        };

        if let Some(timeout) = self.remote_prover_timeout_ms {
            cli_config.remote_prover_timeout = Duration::from_millis(timeout);
        }

        cli_config.note_transport = if let Some(rpc) = &self.note_transport_endpoint {
            Some(NoteTransportConfig {
                endpoint: rpc.clone(),
                ..Default::default()
            })
        } else {
            // Auto-configure note transport for known networks
            match &self.network {
                None | Some(Network::Testnet) => Some(NoteTransportConfig::default()),
                Some(Network::Devnet) => Some(NoteTransportConfig::devnet()),
                Some(Network::Localhost | Network::Custom(_)) => None,
            }
        };

        cli_config.max_block_number_delta = self.block_delta;

        let config_as_toml_string = toml::to_string_pretty(&cli_config).map_err(|err| {
            CliError::Config("failed to serialize config".to_string().into(), err.to_string())
        })?;

        let mut file_handle = File::options()
            .write(true)
            .create_new(true)
            .open(&config_file_path)
            .map_err(|err| {
                CliError::Config("failed to create config file".to_string().into(), err.to_string())
            })?;

        // Resolve package directory relative to .miden directory before writing files
        let config_dir = config_file_path.parent().unwrap();
        let resolved_package_dir = if cli_config.package_directory.is_relative() {
            config_dir.join(&cli_config.package_directory)
        } else {
            cli_config.package_directory.clone()
        };
        write_packages_files(&resolved_package_dir)?;

        file_handle.write_all(config_as_toml_string.as_bytes()).map_err(|err| {
            CliError::Config("failed to write config file".to_string().into(), err.to_string())
        })?;

        println!(
            "Config file successfully created at: {} ({})",
            config_file_path.display(),
            config_type
        );

        Ok(())
    }
}

/// Creates the directory specified by `packages_dir` and writes the `DEFAULT_INCLUDED_PACKAGES`.
fn write_packages_files(packages_dir: &PathBuf) -> Result<(), CliError> {
    fs::create_dir_all(packages_dir).map_err(|err| {
        CliError::Config(
            Box::new(err),
            "failed to create account component templates directory".into(),
        )
    })?;

    for package in DEFAULT_INCLUDED_PACKAGES {
        let package_path = packages_dir.join(package.0);

        // Create parent directory if it doesn't exist (for subdirectories like auth/)
        if let Some(parent) = package_path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                CliError::Config(
                    Box::new(err),
                    format!("Failed to create directory {}", parent.display()),
                )
            })?;
        }

        let mut lib_file = File::create(&package_path).map_err(|err| {
            CliError::Config(
                Box::new(err),
                format!("Failed to create file at {}", package_path.display()),
            )
        })?;
        lib_file.write_all(package.1).map_err(|err| {
            CliError::Config(
                Box::new(err),
                format!(
                    "Failed to write package {} into file {}",
                    package.0,
                    package_path.display()
                ),
            )
        })?;
    }

    info!("Packages files successfully created in: {:?}", packages_dir);

    Ok(())
}

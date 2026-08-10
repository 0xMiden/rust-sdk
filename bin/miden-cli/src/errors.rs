// Suppress false positive from miette+thiserror proc-macro interaction.
#![allow(unused_assignments)]

use std::error::Error;

use miden_client::account::{AccountId, AddressError};
use miden_client::keystore::KeyStoreError;
use miden_client::{
    AccountError,
    AccountIdError,
    AssetError,
    ClientError,
    CodeBuilderError,
    ErrorHint,
    NetworkIdError,
};
use miette::Diagnostic;
use thiserror::Error;

use crate::client_binary_name;
type SourceError = Box<dyn Error + Send + Sync>;

#[derive(Debug, Diagnostic, Error)]
pub enum CliError {
    #[error("account error: {1}")]
    #[diagnostic(code(cli::account_error))]
    Account(#[source] AccountError, String),
    #[error("account component error: {1}")]
    #[diagnostic(code(cli::account_error))]
    AccountComponentError(#[source] SourceError, String),
    #[error("account id error: {1}")]
    #[diagnostic(code(cli::accountid_error), help("Check the account ID format."))]
    AccountId(#[source] AccountIdError, String),
    #[error("address error: {1}")]
    #[diagnostic(code(cli::address_error), help("Check the address format."))]
    Address(#[source] AddressError, String),
    #[error("asset error")]
    #[diagnostic(code(cli::asset_error))]
    Asset(#[source] AssetError),
    #[error("client error")]
    #[diagnostic(code(cli::client_error))]
    Client {
        #[source]
        error: ClientError,
        #[help]
        help: Option<String>,
    },
    #[error("config error: {1}")]
    #[diagnostic(
        code(cli::config_error),
        help(
            "Check if the configuration file exists and is well-formed. If it does not exist, run `{} init` command to create it.",
            client_binary_name().display()
        )
    )]
    Config(#[source] SourceError, String),
    #[error("configuration file not found: {0}")]
    #[diagnostic(
        code(cli::config_not_found),
        help(
            "Run `{} init` command to create a configuration file.",
            client_binary_name().display()
        )
    )]
    ConfigNotFound(String),
    #[error("configuration file already exists: {0}")]
    #[diagnostic(
        code(cli::config_already_exists),
        help(
            "Edit the configuration file, or remove it with `{} clear-config{1}` \
            (this deletes the whole .miden directory, including the store and keystore).",
            client_binary_name().display(),
        )
    )]
    ConfigAlreadyExists(String, String),
    #[error("execute program error: {1}")]
    #[diagnostic(code(cli::execute_program_error))]
    Exec(#[source] SourceError, String),
    #[error("export error: {0}")]
    #[diagnostic(code(cli::export_error), help("Check the ID."))]
    Export(String),
    #[error("faucet error: {1}")]
    #[diagnostic(code(cli::faucet_error))]
    Faucet(#[source] SourceError, String),
    #[error("import error: {0}")]
    #[diagnostic(code(cli::import_error), help("Check the file name."))]
    Import(String),
    #[error("init data error: {1}")]
    #[diagnostic(code(cli::account_error))]
    InitDataError(#[source] SourceError, String),
    #[error("input error: {0}")]
    #[diagnostic(code(cli::input_error))]
    Input(String),
    #[error("io error")]
    #[diagnostic(code(cli::io_error))]
    IO(#[from] std::io::Error),
    #[error("internal error")]
    Internal(#[source] SourceError),
    #[error("keystore error")]
    #[diagnostic(code(cli::keystore_error))]
    KeyStore(#[source] KeyStoreError),
    #[error("missing flag: {0}")]
    #[diagnostic(code(cli::config_error), help("Check the configuration file format."))]
    MissingFlag(String),
    #[error("network id error")]
    NetworkIdError(#[from] NetworkIdError),
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    #[error("parse error: {1}")]
    #[diagnostic(code(cli::parse_error), help("Check the inputs."))]
    Parse(#[source] SourceError, String),
    #[error("replay snapshot error")]
    #[diagnostic(code(cli::replay_snapshot_error))]
    ReplaySnapshot(#[source] SourceError),
    #[error("script builder error")]
    #[diagnostic(code(cli::script_builder_error))]
    CodeBuilder(#[from] CodeBuilderError),
    #[error("transaction error: {1}")]
    #[diagnostic(code(cli::transaction_error))]
    Transaction(#[source] SourceError, String),
    #[error("expected full account, but got partial account: {0}")]
    InvalidAccount(AccountId),
}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        let help = Option::<ErrorHint>::from(&error).map(ErrorHint::into_help_message);
        CliError::Client { error, help }
    }
}

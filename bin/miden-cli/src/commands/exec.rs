use std::collections::BTreeMap;
#[cfg(feature = "dap")]
use std::net::SocketAddr;
use std::path::PathBuf;
use std::slice;

use clap::Parser;
use miden_client::account::AccountId;
use miden_client::keystore::Keystore;
use miden_client::transaction::{ForeignAccount, TransactionScript};
use miden_client::vm::{AdviceInputs, MIN_STACK_DEPTH};
use miden_client::{Client, Felt};

use crate::advice_inputs::load_advice_map_from_file;
use crate::commands::new_account::load_packages;
use crate::config::CliConfig;
use crate::errors::CliError;
use crate::utils::{
    get_input_acc_id_by_prefix_or_default,
    print_executed_program_stack,
    print_executed_program_stack_hex_words,
};

// EXEC COMMAND
// ================================================================================================

#[derive(Debug, Clone, Parser)]
#[command(about = "Execute the specified program against the specified account")]
pub struct ExecCmd {
    /// Account ID to use for the program execution
    #[arg(short = 'a', long = "account")]
    account_id: Option<String>,

    /// Compiled transaction script package (.masp), given as a path or a package name.
    ///
    /// A bare name resolves in the configured package directory.
    #[arg(long, short)]
    package: PathBuf,

    /// Path to a TOML file with advice map entries used as inputs to the VM's advice map.
    #[arg(long, short, long_help = crate::advice_inputs::INPUTS_PATH_LONG_HELP)]
    inputs_path: Option<PathBuf>,

    /// Print the output stack grouped into words
    #[arg(long, default_value_t = false)]
    hex_words: bool,

    /// Start a DAP debug adapter server on the given address (e.g. "127.0.0.1:4711") and wait for a
    /// DAP client to connect before executing.
    #[cfg(feature = "dap")]
    #[arg(long = "start-debug-adapter")]
    start_debug_adapter: Option<SocketAddr>,

    /// Write a replay snapshot of the debug session to this file once it ends.
    ///
    /// The snapshot captures the program, its inputs, the resolved code, and the advice mutations
    /// produced by the transaction host's event handlers, so the same execution can be replayed
    /// offline with `miden-debug --replay <FILE>`. Only meaningful together with
    /// `--start-debug-adapter`.
    #[cfg(feature = "dap")]
    #[arg(long = "record", value_name = "FILE", requires = "start_debug_adapter")]
    record: Option<PathBuf>,
}

impl ExecCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let cli_config = CliConfig::load()?;
        let tx_script = load_tx_script_package(&cli_config, &self.package)?;

        let account_id =
            get_input_acc_id_by_prefix_or_default(&client, self.account_id.clone()).await?;

        let inputs = match &self.inputs_path {
            Some(input_file) => load_advice_map_from_file(input_file)?,
            None => vec![],
        };

        let advice_inputs = AdviceInputs::default().with_map(inputs);

        let output_stack = self
            .execute_program(&client, &cli_config, account_id, tx_script, advice_inputs)
            .await?;

        println!("Program executed successfully");
        if self.hex_words {
            print_executed_program_stack_hex_words(&output_stack);
        } else {
            print_executed_program_stack(&output_stack, None);
        }
        Ok(())
    }

    async fn execute_program<AUTH: Keystore + Sync + 'static>(
        &self,
        client: &Client<AUTH>,
        cli_config: &CliConfig,
        account_id: AccountId,
        tx_script: TransactionScript,
        advice_inputs: AdviceInputs,
    ) -> Result<[Felt; MIN_STACK_DEPTH], CliError> {
        let foreign_accounts = BTreeMap::<AccountId, ForeignAccount>::new();
        #[cfg(not(feature = "dap"))]
        let _ = cli_config;

        #[cfg(feature = "dap")]
        if let Some(addr) = self.start_debug_adapter.as_ref() {
            let mut config = miden_debug::DapConfig::new(addr.to_string());
            // The DAP executor is created and consumed inside the transaction executor, so the
            // advice mutations recorded during the session are read through this shared handle once
            // execution returns.
            let recorder = config.record_event_mutations();
            // When requested, the executor also writes a self-contained replay snapshot of the
            // session (program, inputs, resolved code, and event log) to the given path, so the
            // transaction can be replayed offline with `miden-debug --replay <FILE>`.
            let snapshot_recorder = self
                .record
                .as_deref()
                .map(|path| (config.record_snapshot(path.to_path_buf()), path));
            let config_handle = config.clone();
            miden_debug::DapConfig::set_global(config);

            let mut tx_script = tx_script;
            loop {
                let result = client
                    .execute_program_with_dap(
                        account_id,
                        tx_script,
                        advice_inputs.clone(),
                        foreign_accounts.clone(),
                    )
                    .await;

                if config_handle.restart_requested() {
                    config_handle.reset_restart();
                    tx_script = load_tx_script_package(cli_config, &self.package)?;
                    println!("Reloading package and restarting debug session...");
                    continue;
                }

                // The recording describes the final run of the session and is what an event-replay
                // debug session needs to re-execute this transaction without the live transaction
                // host.
                let mutation_sets = recorder.take();
                if !mutation_sets.is_empty() {
                    println!(
                        "Recorded {} advice mutation set(s) from event handlers during the \
                         debug session.",
                        mutation_sets.len()
                    );
                }
                if let Some((snapshot_recorder, path)) = &snapshot_recorder
                    && let Err(err) = super::report_replay_snapshot_write(snapshot_recorder, path)
                {
                    if result.is_err() {
                        eprintln!("{err}");
                    } else {
                        return Err(err);
                    }
                }
                return result.map_err(|err| {
                    CliError::Exec(err.into(), "error executing the program".to_string())
                });
            }
        }

        client
            .execute_program(account_id, tx_script, advice_inputs, foreign_accounts)
            .await
            .map_err(|err| CliError::Exec(err.into(), "error executing the program".to_string()))
    }
}

/// Loads a compiled transaction script from a package file.
///
/// `TransactionScript::from_package` takes a library whose single `@transaction_script` procedure
/// becomes the entrypoint, which is what `cargo miden build` produces for a `#[tx_script]`. An
/// executable package is rejected, so `--package` is not a way to run a compiled program.
fn load_tx_script_package(
    cli_config: &CliConfig,
    path: &PathBuf,
) -> Result<TransactionScript, CliError> {
    let package = load_packages(cli_config, slice::from_ref(path))?
        .pop()
        .expect("load_packages returns one package per path");

    TransactionScript::from_package(&package).map_err(|err| {
        CliError::Exec(
            err.into(),
            format!("the package at {} is not a transaction script", path.display()),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::env::temp_dir;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::Arc;

    use clap::Parser;
    use miden_client::Serializable;
    use miden_client::assembly::{Assembler, DefaultSourceManager, Module, ModuleKind, Path};

    use super::{CliConfig, ExecCmd, load_tx_script_package};

    #[test]
    fn requires_a_package_and_rejects_source_arguments() {
        assert!(ExecCmd::try_parse_from(["exec"]).is_err());
        assert!(ExecCmd::try_parse_from(["exec", "--script-path", "script.masm"]).is_err());
        assert!(ExecCmd::try_parse_from(["exec", "-s", "script.masm"]).is_err());
        assert!(ExecCmd::try_parse_from(["exec", "--package", "script.masp"]).is_ok());
        assert!(ExecCmd::try_parse_from(["exec", "-p", "script"]).is_ok());
    }

    #[cfg(feature = "dap")]
    #[test]
    fn accepts_a_package_with_debug_adapter_and_recording() {
        assert!(
            ExecCmd::try_parse_from([
                "exec",
                "--package",
                "script.masp",
                "--start-debug-adapter",
                "127.0.0.1:4711",
                "--record",
                "session.mdsnap",
            ])
            .is_ok()
        );
        assert!(
            ExecCmd::try_parse_from([
                "exec",
                "--package",
                "script.masp",
                "--record",
                "session.mdsnap",
            ])
            .is_err()
        );
    }

    /// Assembles `source` into a library package and writes it to a temporary `.masp` file.
    fn write_library_package(name: &str, source: &str) -> PathBuf {
        let source_manager = Arc::new(DefaultSourceManager::default());
        let module = Module::parser(Some(ModuleKind::Library))
            .parse_str(Some(Path::new("exec::test")), source, source_manager.clone())
            .unwrap();
        let package = Assembler::new(source_manager)
            .assemble_library("exec-test", module, None::<&str>)
            .unwrap();

        let path = temp_dir().join(format!("exec-test-{}-{name}.masp", std::process::id()));
        fs::write(&path, package.to_bytes()).unwrap();
        path
    }

    #[test]
    fn loads_transaction_script_package() {
        let path = write_library_package(
            "with-attribute",
            "@transaction_script\npub proc main\n    push.1 drop\nend\n",
        );
        let result = load_tx_script_package(&CliConfig::default(), &path);
        fs::remove_file(&path).unwrap();

        let script = result.unwrap();
        assert!(script.loaded_mast_forest().package_debug_info().unwrap().is_some());
    }

    #[test]
    fn loads_packages_by_name_and_reloads_changed_artifacts() {
        let path = write_library_package(
            "reload",
            "@transaction_script\npub proc main\n    push.1 drop\nend\n",
        );
        let config = CliConfig {
            package_directory: path.parent().unwrap().to_path_buf(),
            ..CliConfig::default()
        };
        let name = PathBuf::from(path.file_stem().unwrap());
        let first = load_tx_script_package(&config, &name).unwrap();
        write_library_package(
            "reload",
            "@transaction_script\npub proc main\n    push.2 drop\nend\n",
        );
        let second = load_tx_script_package(&config, &name).unwrap();
        assert_ne!(first.root(), second.root());
        fs::remove_file(&path).unwrap();
        assert!(load_tx_script_package(&config, &name).is_err());
    }

    #[test]
    fn rejects_source_files_as_packages() {
        assert!(
            load_tx_script_package(&CliConfig::default(), &PathBuf::from("script.masm"),).is_err()
        );
    }

    #[test]
    fn rejects_package_without_transaction_script() {
        let path =
            write_library_package("without-attribute", "pub proc main\n    push.1 drop\nend\n");
        let result = load_tx_script_package(&CliConfig::default(), &path);
        fs::remove_file(&path).unwrap();

        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("is not a transaction script"),
            "unexpected error: {err}"
        );
    }
}

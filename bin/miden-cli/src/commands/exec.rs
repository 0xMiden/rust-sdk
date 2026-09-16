use std::collections::BTreeMap;
#[cfg(feature = "dap")]
use std::net::SocketAddr;
use std::path::PathBuf;
use std::slice;

use clap::{ArgGroup, Parser};
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
#[command(group(ArgGroup::new("script").required(true).args(["script_path", "package"])))]
pub struct ExecCmd {
    /// Account ID to use for the program execution
    #[arg(short = 'a', long = "account")]
    account_id: Option<String>,

    /// Path to script's source code to be executed. Either this or `--package` must be given.
    #[arg(long, short)]
    script_path: Option<String>,

    /// Path to a compiled transaction script package (`.masp`), such as the one `cargo miden build`
    /// produces for a `#[tx_script]`. Either this or `--script-path` must be given.
    ///
    /// A path without an extension is resolved to `<path>.masp` in the configured package
    /// directory, as `call --package` does.
    #[cfg_attr(feature = "dap", arg(long, short, conflicts_with = "start_debug_adapter"))]
    #[cfg_attr(not(feature = "dap"), arg(long, short))]
    package: Option<PathBuf>,

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
        let script_source = self.script_source()?;

        let account_id =
            get_input_acc_id_by_prefix_or_default(&client, self.account_id.clone()).await?;

        let inputs = match &self.inputs_path {
            Some(input_file) => load_advice_map_from_file(input_file)?,
            None => vec![],
        };

        let advice_inputs = AdviceInputs::default().with_map(inputs);

        let tx_script = match &script_source {
            // Pass the path rather than the source string so the assembler's source manager records
            // the real filesystem URI in every `AssemblyOp`'s location. Without this, DAP clients
            // (VS Code, Zed) get `Source { path: None }` in stack traces and can't highlight the
            // current line or open the file.
            ScriptSource::Masm(path) => client.code_builder().compile_tx_script(path.as_path())?,
            ScriptSource::Package(path) => load_tx_script_package(&CliConfig::load()?, path)?,
        };

        let output_stack =
            self.execute_program(&client, account_id, tx_script, advice_inputs).await?;

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
        account_id: AccountId,
        tx_script: TransactionScript,
        advice_inputs: AdviceInputs,
    ) -> Result<[Felt; MIN_STACK_DEPTH], CliError> {
        let foreign_accounts = BTreeMap::<AccountId, ForeignAccount>::new();

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

            // `--package` conflicts with `--start-debug-adapter`, so the source path is present
            // here. A compiled package could not be recompiled when the debug session restarts.
            let script_path = self.script_path.as_deref().map(PathBuf::from).ok_or_else(|| {
                CliError::Input(
                    "the debug adapter needs --script-path, a compiled package cannot be \
                     recompiled on restart"
                        .to_string(),
                )
            })?;
            loop {
                // DAP restart can happen after the user edits the script. Refresh the cached source
                // before compiling again so execution uses the current file contents.
                reload_source_file(&client.source_manager(), script_path.as_path())?;

                let tx_script = client.code_builder().compile_tx_script(script_path.as_path())?;

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
                    println!("Recompiling from source and restarting debug session...");
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

    /// Returns where the transaction script comes from, checking that a source path exists.
    fn script_source(&self) -> Result<ScriptSource, CliError> {
        match (&self.script_path, &self.package) {
            (Some(script_path), None) => {
                let path = PathBuf::from(script_path);
                if !path.exists() {
                    return Err(CliError::Exec(
                        "error with the program file".to_string().into(),
                        format!("the program file at path {script_path} does not exist"),
                    ));
                }
                Ok(ScriptSource::Masm(path))
            },
            (None, Some(package)) => Ok(ScriptSource::Package(package.clone())),
            _ => Err(CliError::Input(
                "exactly one of --script-path or --package must be given".to_string(),
            )),
        }
    }
}

/// Where the transaction script passed to `exec` comes from.
enum ScriptSource {
    /// MASM source, compiled before execution.
    Masm(PathBuf),
    /// A compiled transaction script package (`.masp`).
    Package(PathBuf),
}

/// Loads a compiled transaction script from a package file.
///
/// `TransactionScript::from_package` accepts either an executable package or a library whose single
/// `@transaction_script` procedure becomes the entrypoint, which covers what `cargo miden build`
/// produces for a `#[tx_script]`.
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

// SOURCE FILE RELOADING
// ================================================================================================

#[cfg(feature = "dap")]
use source_reload::reload_source_file;

#[cfg(feature = "dap")]
mod source_reload {
    use std::path::Path;
    use std::sync::Arc;

    use miden_client::assembly::{SourceManagerExt, SourceManagerSync, Uri};

    use crate::errors::CliError;

    /// Reloads a source file from disk into the given source manager.
    ///
    /// Source managers cache files by URI, so compiling a path that has already been loaded may
    /// reuse the cached `SourceFile`. This updates an existing entry for `path` in-place, or loads
    /// it if the source manager has not seen it yet.
    pub(super) fn reload_source_file(
        source_manager: &Arc<dyn SourceManagerSync>,
        path: &Path,
    ) -> Result<(), CliError> {
        let reload_err = |source: Box<dyn std::error::Error + Send + Sync>| {
            CliError::Exec(source, "error reloading the program source file".to_string())
        };

        let uri = Uri::from(path);

        let Some(source_id) = source_manager.find(&uri) else {
            source_manager.load_file(path).map_err(|source| reload_err(Box::new(source)))?;
            return Ok(());
        };

        let source =
            std::fs::read_to_string(path).map_err(|source| reload_err(Box::new(source)))?;
        let version = source_manager
            .get(source_id)
            .map_err(|source| reload_err(Box::new(source)))?
            .content()
            .version()
            .saturating_add(1);

        source_manager
            .update(source_id, source, None, version)
            .map_err(|source| reload_err(Box::new(source)))
    }
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
    fn accepts_either_script_path_or_package() {
        assert!(ExecCmd::try_parse_from(["exec", "--script-path", "script.masm"]).is_ok());
        assert!(ExecCmd::try_parse_from(["exec", "--package", "script.masp"]).is_ok());
    }

    #[test]
    fn rejects_both_script_path_and_package() {
        assert!(
            ExecCmd::try_parse_from([
                "exec",
                "--script-path",
                "script.masm",
                "--package",
                "script.masp"
            ])
            .is_err()
        );
    }

    #[test]
    fn requires_script_path_or_package() {
        assert!(ExecCmd::try_parse_from(["exec"]).is_err());
    }

    #[cfg(feature = "dap")]
    #[test]
    fn package_conflicts_with_debug_adapter() {
        assert!(
            ExecCmd::try_parse_from([
                "exec",
                "--package",
                "script.masp",
                "--start-debug-adapter",
                "127.0.0.1:4711",
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

        result.unwrap();
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

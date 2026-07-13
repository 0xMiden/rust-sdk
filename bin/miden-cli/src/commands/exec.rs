use std::collections::BTreeMap;
#[cfg(feature = "dap")]
use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use miden_client::account::AccountId;
use miden_client::keystore::Keystore;
use miden_client::transaction::{ForeignAccount, TransactionScript};
use miden_client::vm::{AdviceInputs, MIN_STACK_DEPTH};
use miden_client::{Client, Felt};

use crate::advice_inputs::load_advice_map_from_file;
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

    /// Path to script's source code to be executed
    #[arg(long, short)]
    script_path: String,

    /// Path to a TOML file with advice map entries used as inputs to the VM's advice map.
    #[arg(long, short, long_help = crate::advice_inputs::INPUTS_PATH_LONG_HELP)]
    inputs_path: Option<PathBuf>,

    /// Print the output stack grouped into words
    #[arg(long, default_value_t = false)]
    hex_words: bool,

    /// Start a DAP debug adapter server on the given address (e.g. "127.0.0.1:4711")
    /// and wait for a DAP client to connect before executing.
    #[cfg(feature = "dap")]
    #[arg(long = "start-debug-adapter")]
    start_debug_adapter: Option<SocketAddr>,
}

impl ExecCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        let script_path = PathBuf::from(&self.script_path);
        if !script_path.exists() {
            return Err(CliError::Exec(
                "error with the program file".to_string().into(),
                format!("the program file at path {} does not exist", self.script_path),
            ));
        }

        let account_id =
            get_input_acc_id_by_prefix_or_default(&client, self.account_id.clone()).await?;

        let inputs = match &self.inputs_path {
            Some(input_file) => load_advice_map_from_file(input_file)?,
            None => vec![],
        };

        let advice_inputs = AdviceInputs::default().with_map(inputs);

        // Pass the path rather than the source string so the assembler's source manager
        // records the real filesystem URI in every `AssemblyOp`'s location. Without this,
        // DAP clients (VS Code, Zed) get `Source { path: None }` in stack traces and can't
        // highlight the current line or open the file.
        let tx_script = client.code_builder().compile_tx_script(script_path.as_path())?;

        let output_stack =
            self.execute_program(&mut client, account_id, tx_script, advice_inputs).await?;

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
        client: &mut Client<AUTH>,
        account_id: AccountId,
        tx_script: TransactionScript,
        advice_inputs: AdviceInputs,
    ) -> Result<[Felt; MIN_STACK_DEPTH], CliError> {
        let foreign_accounts = BTreeMap::<AccountId, ForeignAccount>::new();

        #[cfg(feature = "dap")]
        if let Some(addr) = self.start_debug_adapter.as_ref() {
            let config = miden_debug::DapConfig::new(addr.to_string());
            let config_handle = config.clone();
            miden_debug::DapConfig::set_global(config);

            let script_path = PathBuf::from(&self.script_path);
            loop {
                // DAP restart can happen after the user edits the script. Refresh the cached
                // source before compiling again so execution uses the current file contents.
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

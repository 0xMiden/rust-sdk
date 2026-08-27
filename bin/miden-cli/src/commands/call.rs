use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::slice;

use clap::Parser;
use miden_client::account::AccountId;
use miden_client::assembly::CodeBuilder;
use miden_client::keystore::Keystore;
use miden_client::rpc::domain::account::AccountStorageRequirements;
use miden_client::transaction::{
    AdviceInputs,
    ForeignAccount,
    TransactionRequestBuilder,
    TransactionRequestError,
    TransactionScript,
    build_fpi_script,
};
use miden_client::vm::{MIN_STACK_DEPTH, Package, PackageExport};
use miden_client::{Client, Felt, Word};

use crate::advice_inputs::load_advice_map_from_file;
use crate::commands::account::DEFAULT_ACCOUNT_ID_KEY;
use crate::commands::new_account::load_packages;
use crate::config::CliConfig;
use crate::errors::CliError;
use crate::utils::{
    parse_account_id,
    print_executed_program_stack,
    print_executed_transaction,
    split_procedure_target,
};

// CALL COMMAND
// ================================================================================================

#[derive(Debug, Clone, Parser)]
#[command(
    about = "Call a procedure on an account and display the result and state delta. Accounts \
             that aren't tracked locally are read from the network and the call is read-only."
)]
pub struct CallCmd {
    /// Account and procedure in the form `<ACCOUNT_ID>:<PROCEDURE>`.
    #[arg(
        value_name = "ACCOUNT_ID:PROCEDURE",
        long_help = "Account and procedure in the form `<ACCOUNT_ID>:<PROCEDURE>`.\n\n\
                     The procedure name is matched against the package's exports with `_` and `-` \
                     treated as equivalent, so it can be written in either snake_case or \
                     kebab-case (e.g. `get_count` matches the WIT export `get-count`)."
    )]
    target: String,

    /// Positional arguments to push onto the stack before calling the procedure.
    #[arg(value_name = "args")]
    args: Vec<String>,

    /// Path to the package (.masp) file containing the procedure. If omitted, `<PROCEDURE>` must
    /// be a hex digest and the output stack is shown as raw felts.
    #[arg(long, short)]
    package: Option<PathBuf>,

    /// Path to a TOML file with advice map entries, in the same format as the `exec` command.
    #[arg(long, short, long_help = crate::advice_inputs::INPUTS_PATH_LONG_HELP)]
    inputs_path: Option<PathBuf>,
}

impl CallCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        client: Client<AUTH>,
    ) -> Result<(), CliError> {
        if client.get_sync_height().await? == 0.into() {
            return Err(CliError::InvalidArgument(
                "Client has not been synced yet. Run `miden-client sync` first.".to_string(),
            ));
        }

        let cli_config = CliConfig::load()?;
        let (account_str, procedure) = split_procedure_target(&self.target);
        let procedure = procedure.ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "Expected `<ACCOUNT_ID>:<PROCEDURE>`, got '{}'.",
                self.target
            ))
        })?;

        let target_id = parse_account_id(&client, account_str).await?;
        let args = parse_args(&self.args)?;
        let call_code = self.resolve_call_code(&client, &cli_config, procedure, &args)?;

        let advice_entries = match &self.inputs_path {
            Some(path) => load_advice_map_from_file(path)?,
            None => vec![],
        };

        let call_target = resolve_call_target(&client, target_id).await?;

        match call_target {
            CallTarget::Local(account_id) => {
                run_local_call(&client, account_id, call_code, &args, advice_entries).await
            },
            CallTarget::Remote { target_id, executor_id, foreign_account } => {
                run_remote_call(
                    &client,
                    target_id,
                    executor_id,
                    foreign_account,
                    call_code,
                    &args,
                    advice_entries,
                )
                .await
            },
        }
    }

    /// Resolves the procedure digest and code builder either from `--package` (calling by name)
    /// or from a hex digest when no package is given.
    fn resolve_call_code<AUTH: Keystore + Sync + 'static>(
        &self,
        client: &Client<AUTH>,
        cli_config: &CliConfig,
        procedure: &str,
        args: &[Felt],
    ) -> Result<CallCode, CliError> {
        // A procedure only sees the top MIN_STACK_DEPTH felts of the stack.
        if args.len() > MIN_STACK_DEPTH {
            return Err(CliError::InvalidArgument(format!(
                "A procedure takes at most {MIN_STACK_DEPTH} input values; got {}.",
                args.len()
            )));
        }

        if let Some(pkg_path) = &self.package {
            let package = load_packages(cli_config, slice::from_ref(pkg_path))?
                .pop()
                .expect("load_packages returns one package per path");
            let digest = resolve_procedure_digest(&package, procedure)?;
            let ProcedureSignature { param_felts, result_felts } =
                print_manifest_signature(&package, procedure);

            match param_felts {
                Some(expected) if args.len() != expected => {
                    return Err(CliError::InvalidArgument(format!(
                        "Procedure '{procedure}' expects {expected} value(s), got {}. Types wider \
                         than one field element are passed as one value per element, as shown in \
                         the signature above.",
                        args.len()
                    )));
                },
                None => {
                    println!(
                        "Warning: no type info for procedure '{procedure}'. Skipping argument \
                         count check. Passing a wrong number of arguments may cause errors or \
                         wrong results."
                    );
                },
                _ => {},
            }

            // The output stack only holds MIN_STACK_DEPTH felts.
            if let Some(n) = result_felts
                && n > MIN_STACK_DEPTH
            {
                return Err(CliError::InvalidArgument(format!(
                    "Procedure '{procedure}' returns {n} values; only up to {MIN_STACK_DEPTH} \
                     can be read from the output stack."
                )));
            }

            // The account's code is loaded from the client's store at VM runtime, so the library
            // doesn't need to be embedded in the script. The assembler still needs it at compile
            // time to resolve `call.<digest>` to a known procedure — otherwise it emits a
            // "phantom target" warning. Dynamic linking provides that resolution without
            // embedding the library bytes.
            let builder = client.code_builder().with_dynamically_linked_package(&package)?;
            Ok(CallCode { builder, digest, result_felts })
        } else {
            let digest = Word::try_from(procedure).map_err(|_| {
                CliError::InvalidArgument(format!(
                    "'{procedure}' is not a hex digest. Pass `--package <FILE>.masp` to \
                     call a procedure by name, or give its hex digest to call without a \
                     package."
                ))
            })?;
            println!(
                "No `--package` provided; output will be raw felts. Pass \
                 `--package <FILE>.masp` for typed output."
            );
            Ok(CallCode {
                builder: client.code_builder(),
                digest,
                result_felts: None,
            })
        }
    }
}

// HELPERS
// ================================================================================================

/// Resolved call code: the linked builder, the procedure digest, and the stack width of the
/// results when known.
struct CallCode {
    builder: CodeBuilder,
    digest: Word,
    result_felts: Option<usize>,
}

/// Runs a remote call via FPI. FPI cannot mutate the foreign account, so there is no state delta
/// to compute — only the read phase runs.
async fn run_remote_call<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    target_id: AccountId,
    executor_id: AccountId,
    foreign_account: Box<ForeignAccount>,
    call_code: CallCode,
    args: &[Felt],
    advice_entries: Vec<(Word, Vec<Felt>)>,
) -> Result<(), CliError> {
    let CallCode { builder, digest, result_felts } = call_code;
    let tx_script =
        build_fpi_script(builder, target_id, digest, args).map_err(|err| match err {
            TransactionRequestError::ForeignProcedureInputsTooLong { max, actual } => {
                CliError::InvalidArgument(format!(
                    "A call on an account read from the network takes at most {max} input felts; \
                     got {actual}"
                ))
            },
            other => {
                CliError::Transaction(other.into(), "Failed to build the call script".to_string())
            },
        })?;

    let output_stack = client
        .execute_program(
            executor_id,
            tx_script,
            AdviceInputs::default().with_map(advice_entries),
            BTreeMap::from([(target_id, *foreign_account)]),
        )
        .await?;

    print_executed_program_stack(&output_stack, result_felts);

    println!("\nA call on an account read from the network can only read it; no state delta.");
    Ok(())
}

/// Runs a local call: a read phase for the return values, then a transaction for the state delta.
/// The account runs the call itself, so the procedure may mutate it.
async fn run_local_call<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    account_id: AccountId,
    call_code: CallCode,
    args: &[Felt],
    advice_entries: Vec<(Word, Vec<Felt>)>,
) -> Result<(), CliError> {
    let CallCode { builder, digest, result_felts } = call_code;
    let tx_script = generate_tx_script(builder, &digest, args)?;

    // 1) Read-only execution to get return values.
    let output_stack = client
        .execute_program(
            account_id,
            tx_script.clone(),
            AdviceInputs::default().with_map(advice_entries.clone()),
            BTreeMap::new(),
        )
        .await?;
    print_executed_program_stack(&output_stack, result_felts);

    // 2) Transaction execution to get the state delta.
    let tx_request = TransactionRequestBuilder::new()
        .custom_script(tx_script)
        .extend_advice_map(advice_entries)
        .build()
        .map_err(|err| {
            CliError::Transaction(err.into(), "Failed to build transaction".to_string())
        })?;

    match client.execute_transaction(account_id, tx_request).await {
        Ok(tx_result) => {
            print_executed_transaction(client, tx_result.executed_transaction()).await?;
        },
        Err(e) => println!("\n(Could not compute state delta: {e})"),
    }
    Ok(())
}

/// Resolved call target.
enum CallTarget {
    /// The account is tracked locally, so it runs the call itself and may be mutated by it.
    Local(AccountId),
    /// The account is read from the network and the call runs from a local account.
    Remote {
        target_id: AccountId,
        executor_id: AccountId,
        foreign_account: Box<ForeignAccount>,
    },
}

async fn resolve_call_target<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    target_id: AccountId,
) -> Result<CallTarget, CliError> {
    if let Some((_, status)) = client.get_account_header(target_id).await? {
        // A locked account holds outdated state and is always private, so it can't be read from
        // the network either.
        if status.is_locked() {
            return Err(CliError::InvalidArgument(format!(
                "Account {target_id} is locked: its local state doesn't match the network's, so \
                 the call can't run on it."
            )));
        }

        return Ok(CallTarget::Local(target_id));
    }

    let foreign_account = ForeignAccount::public(target_id, AccountStorageRequirements::default())
        .map_err(|err| match err {
            TransactionRequestError::InvalidForeignAccountId(_) => {
                CliError::InvalidArgument(format!(
                    "Account {target_id} isn't tracked locally and its state isn't public, so it \
                     can't be read from the network."
                ))
            },
            other => CliError::InvalidArgument(format!(
                "Account {target_id} can't be read from the network: {other}"
            )),
        })?;

    let executor_id = pick_local_executor(client).await?;

    println!(
        "Account {target_id} isn't tracked locally; reading its state from the network and \
         running the call from your account {executor_id}."
    );

    Ok(CallTarget::Remote {
        target_id,
        executor_id,
        foreign_account: Box::new(foreign_account),
    })
}

/// Picks the local account the FPI call runs from, preferring the default account.
///
/// Any account works: the script calls the foreign procedure, not the native account's code.
/// Locked accounts are skipped because their local state doesn't match the node's.
async fn pick_local_executor<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
) -> Result<AccountId, CliError> {
    let default_id: Option<AccountId> =
        client.get_setting(DEFAULT_ACCOUNT_ID_KEY.to_string()).await?;
    if let Some(default_id) = default_id
        && let Some((_, status)) = client.get_account_header(default_id).await?
        && !status.is_locked()
    {
        return Ok(default_id);
    }

    let local_accounts = client.get_account_headers().await?;
    local_accounts
        .iter()
        .find(|(_, status)| !status.is_locked())
        .map(|(header, _)| header.id())
        .ok_or_else(|| {
            CliError::InvalidArgument(
                "Calling an account that isn't tracked locally needs one of your own accounts to \
                 run the call from, and none is usable. Create one with `miden-client new-wallet` \
                 and re-run."
                    .to_string(),
            )
        })
}

fn resolve_procedure_digest(package: &Package, procedure_name: &str) -> Result<Word, CliError> {
    // The user passes a bare name (e.g. `get_count`); match it
    // against each export's name without the module path. Export names may be kebab (Rust/WIT) or
    // snake (hand-written MASM bare identifiers), so compare with `_` and `-` treated as equal.
    let target = procedure_name.replace('_', "-");

    let mut available = Vec::new();
    for export in package.manifest.exports() {
        let PackageExport::Procedure(proc) = export else {
            continue;
        };
        if export.name().replace('_', "-") != target {
            // Not the requested procedure; keep it for the "not found" error list.
            available.push(format!("  {}", proc.path));
            continue;
        }
        // The same leaf name is exported both as a `C`-ABI lowering (for `exec`) and as the
        // `ComponentModel` export (the cross-context `call` target); pick the latter.
        if proc.signature.as_ref().is_some_and(|sig| sig.abi.is_wasm_canonical_abi()) {
            return Ok(proc.digest);
        }
    }

    Err(CliError::InvalidArgument(format!(
        "Procedure '{procedure_name}' not found. Available:\n{}",
        available.join("\n")
    )))
}

fn parse_args(args: &[String]) -> Result<Vec<Felt>, CliError> {
    args.iter()
        .map(|arg| {
            let n = arg.parse::<u64>().map_err(|_| {
                CliError::InvalidArgument(format!("Invalid argument '{arg}'. Expected u64."))
            })?;
            Felt::try_from(n)
                .map_err(|_| CliError::InvalidArgument(format!("Argument '{arg}' is too large.")))
        })
        .collect()
}

/// How many field elements a procedure's arguments and results occupy on the stack. A multi-felt
/// type such as `Word` counts as its flattened width, not as one item. `None` means the
/// information is unavailable (procedure missing from manifest or export lacks type info).
struct ProcedureSignature {
    param_felts: Option<usize>,
    result_felts: Option<usize>,
}

/// Prints the signature of `procedure_name` from the package manifest and returns the stack width
/// of its arguments and results. If the procedure is missing, prints the list of available exports.
fn print_manifest_signature(package: &Package, procedure_name: &str) -> ProcedureSignature {
    const UNKNOWN: ProcedureSignature =
        ProcedureSignature { param_felts: None, result_felts: None };

    let kebab_name = procedure_name.replace('_', "-");
    let quoted_kebab = format!("\"{kebab_name}\"");
    let quoted_name = format!("\"{procedure_name}\"");

    for export in package.manifest.exports() {
        let PackageExport::Procedure(proc_export) = export else {
            continue;
        };

        let path_str = proc_export.path.to_string();
        if !path_str.ends_with(&kebab_name)
            && !path_str.ends_with(procedure_name)
            && !path_str.ends_with(&quoted_kebab)
            && !path_str.ends_with(&quoted_name)
        {
            continue;
        }

        if let Some(sig) = &proc_export.signature {
            let mut param_felts = Vec::with_capacity(sig.params.len());
            for ty in &sig.params {
                param_felts.push(ty.size_in_felts());
            }
            let mut result_felts = Vec::with_capacity(sig.results.len());
            for ty in &sig.results {
                result_felts.push(ty.size_in_felts());
            }

            println!("Raw Signature: {sig}\n");

            // The stack is flat, so the counts that matter are the flattened widths: a `Word`
            // parameter takes four stack slots, not one.
            return ProcedureSignature {
                param_felts: Some(param_felts.iter().sum()),
                result_felts: Some(result_felts.iter().sum()),
            };
        }
        println!("Raw Signature: {procedure_name}(...) [no type info]\n");
        return UNKNOWN;
    }

    println!("(procedure '{procedure_name}' not found in manifest exports)");
    println!("Available exports:");
    for export in package.manifest.exports() {
        if let PackageExport::Procedure(p) = export {
            println!("  {}", p.path);
        }
    }
    println!();
    UNKNOWN
}

/// Builds a transaction script that pushes `args` and calls the procedure at `digest`.
///
/// Only the top results are read back, and `truncate_stack` restores the 16-element exit
/// invariant, so anything left below the results can stay there.
fn generate_tx_script(
    code_builder: CodeBuilder,
    digest: &Word,
    args: &[Felt],
) -> Result<TransactionScript, CliError> {
    let mut script = String::from("use miden::core::sys\n\n@transaction_script\npub proc main\n");

    // Push args in reverse so the first arg ends up on top.
    for arg in args.iter().rev() {
        writeln!(script, "    push.{arg}").unwrap();
    }

    writeln!(script, "    call.{}", digest.to_hex()).unwrap();

    script.push_str("    exec.sys::truncate_stack\n");
    script.push_str("end\n");
    Ok(code_builder.compile_tx_script(&script)?)
}

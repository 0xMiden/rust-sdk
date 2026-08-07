use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use clap::Parser;
use miden_client::assembly::CodeBuilder;
use miden_client::keystore::Keystore;
use miden_client::transaction::{AdviceInputs, TransactionRequestBuilder, TransactionScript};
use miden_client::vm::{Package, PackageExport};
use miden_client::{Client, Deserializable, Felt, Word};

use crate::advice_inputs::load_advice_map_from_file;
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
#[command(about = "Call a procedure on a local account and display the result and state delta")]
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

    /// Path to the package (.masp) file containing the procedure.
    #[arg(long, short)]
    package: PathBuf,

    /// Path to a TOML file with advice map entries, in the same format as the `exec` command.
    #[arg(long, short, long_help = crate::advice_inputs::INPUTS_PATH_LONG_HELP)]
    inputs_path: Option<PathBuf>,
}

impl CallCmd {
    pub async fn execute<AUTH: Keystore + Sync + 'static>(
        &self,
        mut client: Client<AUTH>,
    ) -> Result<(), CliError> {
        if client.get_sync_height().await? == 0.into() {
            return Err(CliError::InvalidArgument(
                "Client has not been synced yet. Run `miden-client sync` first.".to_string(),
            ));
        }

        let (account_str, procedure) = split_procedure_target(&self.target);
        let procedure = procedure.ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "Expected `<ACCOUNT_ID>:<PROCEDURE>`, got '{}'.",
                self.target
            ))
        })?;

        let account_id = parse_account_id(&client, account_str).await?;
        client.try_get_account(account_id).await?;

        let package = load_package(&self.package)?;

        let digest = resolve_procedure_digest(&package, procedure)?;
        let ProcedureSignature { param_felts, result_felts } =
            print_manifest_signature(&package, procedure);

        let args = parse_args(&self.args)?;

        let advice_entries = match &self.inputs_path {
            Some(path) => load_advice_map_from_file(path)?,
            None => vec![],
        };

        match param_felts {
            Some(expected) if args.len() != expected => {
                return Err(CliError::InvalidArgument(format!(
                    "Procedure '{procedure}' expects {expected} value(s), got {}. Types wider \
                     than one field element are passed as one value per element, as shown in the \
                     signature above.",
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

        // The account's code is loaded into the from the client's store in th VM runtime, so we
        // don't need the library into the compiled script. But the assembler still needs
        // it at compile time to resolve `call.<digest>` to a known procedure — otherwise it
        // emits a "phantom target" warning. Dynamic linking provides that resolution without
        // embedding the library bytes in the script.
        let linked_builder = client.code_builder().with_dynamically_linked_library(&package)?;

        // 1) Read-only execution to get return values. If `result_felts` is unknown we skip
        // the drop sequence and let `print_output_stack` auto-detect results from the stack.
        let read_tx_script =
            generate_tx_script(linked_builder.clone(), &digest, &args, result_felts)?;

        let advice_inputs = AdviceInputs::default().with_map(advice_entries.clone());

        let output_stack = client
            .execute_program(account_id, read_tx_script, advice_inputs, BTreeMap::new())
            .await?;

        print_executed_program_stack(&output_stack, result_felts);

        // 2) Transaction execution to get state delta.
        let delta_tx_script = generate_tx_script(linked_builder, &digest, &args, Some(0))?;

        let tx_request = TransactionRequestBuilder::new()
            .custom_script(delta_tx_script)
            .extend_advice_map(advice_entries)
            .build()
            .map_err(|err| {
                CliError::Transaction(err.into(), "Failed to build transaction".to_string())
            })?;

        match client.execute_transaction(account_id, tx_request).await {
            Ok(tx_result) => {
                print_executed_transaction(&mut client, tx_result.executed_transaction()).await?;
            },
            Err(e) => {
                println!("\n(Could not compute state delta: {e})");
            },
        }

        Ok(())
    }
}

// HELPERS
// ================================================================================================

fn load_package(path: &Path) -> Result<Package, CliError> {
    if !path.exists() {
        return Err(CliError::InvalidArgument(format!(
            "Package file not found: {}",
            path.display()
        )));
    }
    let bytes = std::fs::read(path)?;
    Package::read_from_bytes(&bytes).map_err(|e| {
        CliError::Parse(Box::new(e), format!("Failed to deserialize package: {}", path.display()))
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

/// Builds a transaction script that pushes `args`, calls the procedure at `digest`, and optionally
/// drops the pushed args from under the results. `Some(n)` keeps the top `n` values; `None` skips
/// drops.
fn generate_tx_script(
    code_builder: CodeBuilder,
    digest: &Word,
    args: &[Felt],
    result_count: Option<usize>,
) -> Result<TransactionScript, CliError> {
    // MASM `movup.n` only works for n in 2..=15. The VM stack exposes only the top
    // 16 elements; anything deeper lives in the overflow table and cannot be reached
    // by `movup`. So we can't drop args from under more than 15 results.
    // See miden-vm/docs/src/user_docs/assembly/instruction_reference.md (movup row)
    // and miden-vm/docs/src/design/stack/stack_ops.md (MOVUP/MOVDN sections).
    if let Some(n) = result_count
        && n > 15
    {
        return Err(CliError::InvalidArgument(format!(
            "Procedure returns {n} values; only up to 15 are supported."
        )));
    }

    let mut script = String::from("@transaction_script\npub proc main\n");

    // Push args in reverse so the first arg ends up on top.
    for arg in args.iter().rev() {
        writeln!(script, "    push.{arg}").unwrap();
    }

    writeln!(script, "    call.{}", digest.to_hex()).unwrap();

    let to_drop = args.len();
    if to_drop > 0 {
        match result_count {
            Some(0) => {
                for _ in 0..to_drop {
                    script.push_str("    drop\n");
                }
            },
            Some(1) => {
                for _ in 0..to_drop {
                    script.push_str("    swap drop\n");
                }
            },
            Some(n) => {
                for _ in 0..to_drop {
                    writeln!(script, "    movup.{n} drop").unwrap();
                }
            },
            None => {},
        }
    }

    script.push_str("end\n");
    Ok(code_builder.compile_tx_script(&script)?)
}

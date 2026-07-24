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
use miden_client::vm::{MIN_STACK_DEPTH, PackageExport, PackageManifest, ProcedureExport};
use miden_client::{Client, Felt, Word};
use miden_mast_package::typed::TypedProcInfo;

use crate::advice_inputs::load_advice_map_from_file;
use crate::codecs::with_cli_codecs;
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

/// Part of the transaction kernel's assertion message for a transaction that changes nothing.
///
/// The assertion carries a message rather than a code the executor exposes, so this is matched as
/// text. It only decides whether an explanatory line is printed: if the kernel ever rewords it,
/// the line stops appearing and nothing else changes.
const EMPTY_TRANSACTION_ASSERTION: &str = "neither changed the account state";

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
            return Err(CliError::NotSynced);
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
        let call_code = self.resolve_call_code(&client, &cli_config, procedure)?;

        // The script passes every argument on the stack, and `MIN_STACK_DEPTH` is what the callee
        // sees of it. An argument below that reaches the callee as a zero and the call still
        // succeeds, so without this check a procedure with wide arguments would run on the wrong
        // values.
        if call_code.args.len() > MIN_STACK_DEPTH {
            return Err(CliError::InvalidArgument(format!(
                "The arguments occupy {} stack values, and a called procedure can only see {}.",
                call_code.args.len(),
                MIN_STACK_DEPTH
            )));
        }

        // The output stack only holds MIN_STACK_DEPTH felts.
        if let Some(n) = call_code.result_felts
            && n > MIN_STACK_DEPTH
        {
            return Err(CliError::InvalidArgument(format!(
                "Procedure '{procedure}' returns {n} values; only up to {MIN_STACK_DEPTH} \
                 can be read from the output stack."
            )));
        }

        let advice_entries = match &self.inputs_path {
            Some(path) => load_advice_map_from_file(path)?,
            None => vec![],
        };

        let call_target = resolve_call_target(&client, target_id).await?;

        match call_target {
            CallTarget::Local(account_id) => {
                run_local_call(&client, account_id, call_code, advice_entries).await
            },
            CallTarget::Remote { target_id, executor_id, foreign_account } => {
                run_remote_call(
                    &client,
                    target_id,
                    executor_id,
                    foreign_account,
                    call_code,
                    advice_entries,
                )
                .await
            },
        }
    }

    /// Resolves the procedure digest, code builder and encoded arguments either from `--package`
    /// (calling by name) or from a hex digest when no package is given.
    fn resolve_call_code<AUTH: Keystore + Sync + 'static>(
        &self,
        client: &Client<AUTH>,
        cli_config: &CliConfig,
        procedure: &str,
    ) -> Result<CallCode, CliError> {
        let Some(pkg_path) = &self.package else {
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
            return Ok(CallCode {
                builder: client.code_builder(),
                digest,
                args: parse_raw_args(&self.args)?,
                typed: None,
                result_felts: None,
            });
        };

        let package = load_packages(cli_config, slice::from_ref(pkg_path))?
            .pop()
            .expect("load_packages returns one package per path");

        let export = resolve_procedure_export(&package.manifest, procedure)?;
        let digest = export.digest;
        let signature = export.signature.clone().ok_or_else(|| missing_signature(procedure))?;
        // The signature prints under the name the package carries, not the one the user typed, so
        // `call increment_by` shows `increment-by(felt) -> felt`.
        let name = export.path.last().ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "The export matching '{procedure}' has an empty path, so it names no procedure."
            ))
        })?;
        let typed = with_cli_codecs(TypedProcInfo::new(name, signature));

        println!("Signature: {typed}\n");

        let result_felts = typed.output_felt_count();
        // Checks the argument count as well, and names the procedure and both counts when it is
        // wrong, so there is nothing to check here first.
        let args = typed.encode_args(&self.args)?;

        // The account's code is loaded from the client's store at VM runtime, so the library
        // doesn't need to be embedded in the script. The assembler still needs it at compile
        // time to resolve `call.<digest>` to a known procedure — otherwise it emits a
        // "phantom target" warning. Dynamic linking provides that resolution without
        // embedding the library bytes.
        let builder = client.code_builder().with_dynamically_linked_package(&package)?;
        Ok(CallCode { builder, digest, args, typed: Some(typed), result_felts })
    }
}

// HELPERS
// ================================================================================================

/// Resolved call code: the linked builder, the procedure digest, the encoded arguments, the type
/// information used to render the result when the package describes it, and the stack width of the
/// results when known.
struct CallCode {
    builder: CodeBuilder,
    digest: Word,
    args: Vec<Felt>,
    typed: Option<TypedProcInfo>,
    result_felts: Option<usize>,
}

/// Prints the values the procedure returned, rendered as their declared types when the package
/// describes them and as raw stack felts otherwise.
fn print_call_result(
    output_stack: &[Felt],
    typed: Option<&TypedProcInfo>,
    result_felts: Option<usize>,
) -> Result<(), CliError> {
    match typed {
        // A procedure that returns nothing has no result to show; anything else that cannot be
        // rendered is an error, since a raw stack dump would hide that the result is not a valid
        // value of its type.
        Some(typed) => {
            if let Some(rendered) = typed.decode_result(output_stack)? {
                println!("Result: {rendered}");
            }
        },
        None => print_executed_program_stack(output_stack, result_felts),
    }
    Ok(())
}

/// Runs a remote call via FPI. FPI cannot mutate the foreign account, so there is no state delta
/// to compute — only the read phase runs.
async fn run_remote_call<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    target_id: AccountId,
    executor_id: AccountId,
    foreign_account: Box<ForeignAccount>,
    call_code: CallCode,
    advice_entries: Vec<(Word, Vec<Felt>)>,
) -> Result<(), CliError> {
    let CallCode { builder, digest, args, typed, result_felts } = call_code;
    let tx_script =
        build_fpi_script(builder, target_id, digest, &args).map_err(|err| match err {
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

    print_call_result(output_stack.as_slice(), typed.as_ref(), result_felts)?;

    println!("\nA call on an account read from the network can only read it; no state delta.");
    Ok(())
}

/// Runs a local call: a read phase for the return values, then a transaction for the state delta.
/// The account runs the call itself, so the procedure may mutate it.
async fn run_local_call<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    account_id: AccountId,
    call_code: CallCode,
    advice_entries: Vec<(Word, Vec<Felt>)>,
) -> Result<(), CliError> {
    let CallCode { builder, digest, args, typed, result_felts } = call_code;
    let tx_script = generate_tx_script(builder, &digest, &args)?;

    // 1) Read-only execution to get return values.
    let output_stack = client
        .execute_program(
            account_id,
            tx_script.clone(),
            AdviceInputs::default().with_map(advice_entries.clone()),
            BTreeMap::new(),
        )
        .await?;
    print_call_result(output_stack.as_slice(), typed.as_ref(), result_felts)?;

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
        Err(e) => {
            let mut report = String::new();
            let mut cause = std::error::Error::source(&e);
            while let Some(err) = cause {
                writeln!(report, "  caused by: {err}").unwrap();
                cause = err.source();
            }

            if report.contains(EMPTY_TRANSACTION_ASSERTION) {
                // A procedure that only reads, on an account whose components write nothing,
                // leaves the transaction with no effects at all, and the kernel refuses those.
                // For a read-only call that is the expected outcome rather than a fault, so it
                // is reported instead of dumping the assertion chain. The kernel rejects only
                // when the account was left unchanged and nothing was consumed, so that is all
                // this can report; it says nothing about created notes.
                println!();
                println!("The transaction was rejected because it had no effects:\n");
                println!("No notes were consumed.");
                println!();
                println!("Account Storage was not changed.");
                println!("Account Vault was not changed.");
                println!("Account nonce was not changed.");
            } else {
                println!("\n(Could not compute state delta: {e})");
                print!("{report}");
            }
        },
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

/// Parses the arguments of a call made without a package, where nothing describes their types and
/// each one is one field element.
fn parse_raw_args(args: &[String]) -> Result<Vec<Felt>, CliError> {
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

/// Finds the export `procedure_name` names, which carries both the digest to call and the
/// signature the arguments are encoded against.
///
/// The compiler writes two exports for the same Component Model procedure: one with its WIT
/// signature, `add-points(point, point) -> point`, and one lowered to the C ABI,
/// `fn(felt, felt, felt, felt) -> i32`. Arguments are encoded and results are rendered from the
/// signature this picks, so it has to be the WIT one. The lowered signature describes the ABI
/// plumbing instead: its parameters are the flattened felts, and its `i32` result is a pointer to
/// the value rather than the value.
fn resolve_procedure_export<'a>(
    manifest: &'a PackageManifest,
    procedure_name: &str,
) -> Result<&'a ProcedureExport, CliError> {
    // The user passes a bare name (e.g. `get_count`); match it
    // against each export's name without the module path. Export names may be kebab (Rust/WIT) or
    // snake (hand-written MASM bare identifiers), so compare with `_` and `-` treated as equal.
    let target = procedure_name.replace('_', "-");

    let mut available = Vec::new();
    let mut found_without_signature = false;

    for export in manifest.exports() {
        let PackageExport::Procedure(proc) = export else {
            continue;
        };
        // Every procedure goes on the list, so a "not found" error shows the whole surface.
        available.push(format!("  {}", proc.path));

        if export.name().replace('_', "-") != target {
            continue;
        }
        // The same leaf name is exported both as a `C`-ABI lowering (for `exec`) and as the
        // `ComponentModel` export (the cross-context `call` target); pick the latter.
        if proc.signature.as_ref().is_some_and(|sig| sig.abi.is_wasm_canonical_abi()) {
            return Ok(proc);
        }
        found_without_signature = true;
    }

    if found_without_signature {
        return Err(missing_signature(procedure_name));
    }

    Err(CliError::InvalidArgument(format!(
        "Procedure '{procedure_name}' not found. Available:\n{}",
        available.join("\n")
    )))
}

/// The error for a procedure the package exports but does not describe well enough to call: `call`
/// encodes its arguments from the signature, so a procedure without one cannot be reached.
fn missing_signature(procedure_name: &str) -> CliError {
    CliError::InvalidArgument(format!(
        "Procedure '{procedure_name}' is exported without a type signature, so its arguments \
         cannot be encoded. Only procedures built from a WIT interface carry one."
    ))
}

// TESTS
// ================================================================================================

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use miden_mast_package::PathBuf;
    use midenc_hir_type::{CallConv, FunctionType, Type};

    use super::*;

    /// A manifest exporting every `(path, signature)` pair. Resolution matches on the path and
    /// reads the signature, so the digest is left zero.
    fn manifest_with_exports(exports: &[(&str, Option<FunctionType>)]) -> PackageManifest {
        let exports = exports.iter().map(|(path, signature)| {
            let path: Arc<_> = path.parse::<PathBuf>().expect("path should parse").into();
            PackageExport::Procedure(ProcedureExport::new(
                path,
                None,
                Word::default(),
                signature.clone(),
            ))
        });

        PackageManifest::new(exports).expect("manifest should be valid")
    }

    /// The interface form of a Component Model export. It keeps the WIT types.
    fn interface_form() -> (&'static str, Option<FunctionType>) {
        (
            "::\"miden:counter/counter@0.1.0\"::\"increment-by\"",
            Some(FunctionType::new(CallConv::ComponentModel, [Type::Felt], [Type::Felt])),
        )
    }

    /// The lowered form of the same export. The C ABI flattens the types and returns the big value
    /// by reference: an `i32` pointer, not the value.
    fn lowered_form() -> (&'static str, Option<FunctionType>) {
        (
            "::\"miden:counter/counter@0.1.0\"::cc::\"miden:counter/counter@0.1.0#increment-by\"",
            Some(FunctionType::new(CallConv::C, [Type::Felt], [Type::I32])),
        )
    }

    #[test]
    fn the_interface_form_wins_over_the_lowered_one() {
        // The compiler is free to write the two exports in either order, so neither may decide it.
        for exports in [[interface_form(), lowered_form()], [lowered_form(), interface_form()]] {
            let manifest = manifest_with_exports(&exports);

            let export = resolve_procedure_export(&manifest, "increment-by").unwrap();
            assert_eq!(export.signature, interface_form().1);
        }
    }

    #[test]
    fn a_lowered_name_is_not_reachable_by_the_bare_procedure_name() {
        // The last part of the lowered path holds the whole interface, so it never equals the
        // plain name. Were it found, its `i32` return would be printed as a value.
        let manifest = manifest_with_exports(&[lowered_form()]);

        let err = resolve_procedure_export(&manifest, "increment-by").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid argument: Procedure 'increment-by' not found. Available:\n  \
             ::\"miden:counter/counter@0.1.0\"::cc::\"miden:counter/counter@0.1.0#increment-by\""
        );
    }

    #[test]
    fn an_underscore_query_finds_a_kebab_export() {
        let manifest = manifest_with_exports(&[interface_form(), lowered_form()]);

        let export = resolve_procedure_export(&manifest, "increment_by").unwrap();
        assert_eq!(export.signature, interface_form().1);
    }

    #[test]
    fn a_hand_written_masm_export_does_not_shadow_the_component_model_one() {
        // A MASM `increment_by` matches the query by name, but `call` needs the Component Model
        // signature: only that one describes the values the user passes and reads.
        let masm =
            || ("::mix::increment_by", Some(FunctionType::new(CallConv::Fast, [], [Type::U32])));
        for exports in [[interface_form(), masm()], [masm(), interface_form()]] {
            let manifest = manifest_with_exports(&exports);

            let export = resolve_procedure_export(&manifest, "increment_by").unwrap();
            assert_eq!(export.signature, interface_form().1);
        }
    }

    #[test]
    fn an_unknown_procedure_lists_the_whole_export_surface() {
        let manifest = manifest_with_exports(&[interface_form(), lowered_form()]);

        let err = resolve_procedure_export(&manifest, "no-such-proc").unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid argument: Procedure 'no-such-proc' not found. Available:\n  \
             ::\"miden:counter/counter@0.1.0\"::\"increment-by\"\n  \
             ::\"miden:counter/counter@0.1.0\"::cc::\"miden:counter/counter@0.1.0#increment-by\""
        );
    }

    #[test]
    fn an_export_without_a_signature_is_reported_as_such() {
        // MASM written by hand: the export has the name we ask for, but no type info. The error
        // has to say that, not that the procedure is missing.
        let manifest = manifest_with_exports(&[("::mix::\"increment-by\"", None)]);

        let err = resolve_procedure_export(&manifest, "increment-by").unwrap_err();
        assert_eq!(err.to_string(), missing_signature("increment-by").to_string());
    }
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

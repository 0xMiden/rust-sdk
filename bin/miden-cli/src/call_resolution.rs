use std::fmt::Write as _;
use std::path::PathBuf;
use std::slice;

use miden_client::account::AccountId;
use miden_client::assembly::CodeBuilder;
use miden_client::keystore::Keystore;
use miden_client::note::NoteScript;
use miden_client::transaction::TransactionScript;
use miden_client::vm::typed::TypedProcInfo;
use miden_client::vm::{MIN_STACK_DEPTH, PackageExport, PackageManifest, ProcedureExport};
use miden_client::{Client, Felt, Word};

use crate::advice_inputs::load_advice_map_from_file;
use crate::codecs::with_cli_codecs;
use crate::config::CliConfig;
use crate::errors::CliError;
use crate::packages::load_packages;
use crate::utils::{parse_account_id, split_procedure_target};

// CALL RESOLUTION
// ================================================================================================

/// A call target, procedure and arguments, resolved and ready to run.
pub(crate) struct ResolvedCall {
    pub(crate) target_id: AccountId,
    pub(crate) call_code: CallCode,
    pub(crate) advice_entries: Vec<(Word, Vec<Felt>)>,
}

/// Resolves `<ACCOUNT_ID>:<PROCEDURE>`, the arguments and the advice inputs of a procedure call.
/// The advice inputs include the scripts of the `note_scripts` packages, keyed by script root.
///
/// Set `reads_output` when the caller reads the procedure's result from the output stack. It only
/// controls whether a return type wider than the output stack is rejected. A caller that submits a
/// transaction and discards the result passes `false`.
pub(crate) async fn resolve_call<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    target: &str,
    package: Option<&PathBuf>,
    args: &[String],
    inputs_path: Option<&PathBuf>,
    note_scripts: &[PathBuf],
    reads_output: bool,
) -> Result<ResolvedCall, CliError> {
    if client.get_sync_height().await? == 0.into() {
        return Err(CliError::NotSynced);
    }

    let cli_config = CliConfig::load()?;
    let (account_str, procedure) = split_procedure_target(target);
    let procedure = procedure.ok_or_else(|| {
        CliError::InvalidArgument(format!("Expected `<ACCOUNT_ID>:<PROCEDURE>`, got '{target}'."))
    })?;

    let target_id = parse_account_id(client, account_str).await?;
    let call_code = resolve_call_code(client, &cli_config, package, procedure, args, reads_output)?;

    let mut advice_entries = match inputs_path {
        Some(path) => load_advice_map_from_file(path)?,
        None => vec![],
    };

    // The host builds a full output note only when the note script is in the advice map under its
    // root. Without it, a public note fails and a private note keeps only its recipient digest.
    let packages = load_packages(&cli_config, note_scripts)?;
    for (path, package) in note_scripts.iter().zip(&packages) {
        let script = NoteScript::from_package(package).map_err(|err| {
            CliError::InvalidArgument(format!(
                "'{}' is not a note script package: {err}",
                path.display()
            ))
        })?;
        let root = Word::from(script.root());
        println!("Note script {root}: {}", path.display());
        advice_entries.push((root, Vec::<Felt>::from(&script)));
    }

    Ok(ResolvedCall { target_id, call_code, advice_entries })
}

/// Resolves the procedure digest, code builder and encoded arguments either from `--package`
/// (calling by name) or from a hex digest when no package is given.
fn resolve_call_code<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    cli_config: &CliConfig,
    package: Option<&PathBuf>,
    procedure: &str,
    args: &[String],
    reads_output: bool,
) -> Result<CallCode, CliError> {
    let call_code = match package {
        Some(pkg_path) => resolve_from_package(client, cli_config, pkg_path, procedure, args)?,
        None => resolve_from_digest(client, procedure, args)?,
    };

    // A procedure only sees the top MIN_STACK_DEPTH felts of the stack. An argument below that
    // reaches it as a zero and the call still succeeds, so without this check a procedure with wide
    // arguments would run on the wrong values.
    if call_code.args.len() > MIN_STACK_DEPTH {
        return Err(CliError::InvalidArgument(format!(
            "A procedure takes at most {MIN_STACK_DEPTH} input values; got {}.",
            call_code.args.len()
        )));
    }

    // The output stack only holds MIN_STACK_DEPTH felts. Only a caller that reads the result needs
    // it to fit, so a caller that discards the result does not run this check.
    if reads_output
        && let Some(n) = call_code.typed.as_ref().and_then(TypedProcInfo::output_felt_count)
        && n > MIN_STACK_DEPTH
    {
        return Err(CliError::InvalidArgument(format!(
            "Procedure '{procedure}' returns {n} values; only up to {MIN_STACK_DEPTH} \
             can be read from the output stack."
        )));
    }

    Ok(call_code)
}

/// Resolves the call from the package's manifest, which names the procedure and, when it was built
/// from a WIT interface, describes the types its arguments and results are encoded as.
fn resolve_from_package<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    cli_config: &CliConfig,
    pkg_path: &PathBuf,
    procedure: &str,
    args: &[String],
) -> Result<CallCode, CliError> {
    let package = load_packages(cli_config, slice::from_ref(pkg_path))?
        .pop()
        .expect("load_packages returns one package per path");

    let export = resolve_procedure_export(&package.manifest, procedure)?;
    let digest = export.digest;
    // The signature prints under the name the package carries, not the one the user typed, so `call
    // increment_by` shows `increment-by(felt) -> felt`.
    let name = export.path.last().ok_or_else(|| {
        CliError::InvalidArgument(format!(
            "The export matching '{procedure}' has an empty path, so it names no procedure."
        ))
    })?;

    // Only a Component Model signature describes the values the user passes and reads; a `C`-ABI or
    // `Fast` one describes the lowering instead, which would encode the wrong thing.
    let typed = match export.signature.clone() {
        Some(signature) if signature.abi.is_wasm_canonical_abi() => {
            Some(with_cli_codecs(TypedProcInfo::new(name, signature)?))
        },
        _ => None,
    };

    let args = if let Some(typed) = &typed {
        println!("Signature: {typed}\n");
        // Checks the argument count as well, and names the procedure and both counts when it is
        // wrong, so there is nothing to check here first.
        typed.encode_args(args)?
    } else {
        println!("Signature: {name}(...) [no type info]\n");
        println!(
            "Warning: the package does not describe the types of '{procedure}', so each \
             argument is passed as one field element, the argument count is not checked, and \
             the result is printed as a stack dump."
        );
        encode_raw_args(args)?
    };

    // The account's code is loaded from the client's store at VM runtime, so the library doesn't
    // need to be embedded in the script. The assembler still needs it at compile time to resolve
    // `call.<digest>` to a known procedure — otherwise it emits a "phantom target" warning. Dynamic
    // linking provides that resolution without embedding the library bytes.
    let builder = client.code_builder().with_dynamically_linked_package(&package)?;
    Ok(CallCode { builder, digest, args, typed })
}

/// Resolves the call from a hex digest. Nothing describes the procedure, so each argument is one
/// field element and the results are read back as raw stack felts.
fn resolve_from_digest<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    procedure: &str,
    args: &[String],
) -> Result<CallCode, CliError> {
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
        args: encode_raw_args(args)?,
        typed: None,
    })
}

// CALL CODE
// ================================================================================================

/// Resolved call code: the linked builder, the procedure digest, the encoded arguments, and the
/// type information the arguments were encoded against, which the result is rendered with as well.
pub(crate) struct CallCode {
    pub(crate) builder: CodeBuilder,
    pub(crate) digest: Word,
    pub(crate) args: Vec<Felt>,
    pub(crate) typed: Option<TypedProcInfo>,
}

/// Finds the export `procedure_name` names, which carries both the digest to call and the signature
/// the arguments are encoded against.
///
/// The compiler writes two exports for the same Component Model procedure: one with its WIT
/// signature, `add-points(point, point) -> point`, and one lowered to the C ABI, `fn(felt, felt,
/// felt, felt) -> i32`. Arguments are encoded and results are rendered from the signature this
/// picks, so it has to be the WIT one. The lowered signature describes the ABI plumbing instead:
/// its parameters are the flattened felts, and its `i32` result is a pointer to the value rather
/// than the value.
fn resolve_procedure_export<'a>(
    manifest: &'a PackageManifest,
    procedure_name: &str,
) -> Result<&'a ProcedureExport, CliError> {
    // The user passes a bare name (e.g. `get_count`); match it against each export's name without
    // the module path. Export names may be kebab (Rust/WIT) or snake (hand-written MASM bare
    // identifiers), so compare with `_` and `-` treated as equal.
    let target = procedure_name.replace('_', "-");

    let mut available = Vec::new();
    let mut untyped = None;

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
        // Any other match is the fallback: an export carrying no signature at all (hand-written
        // MASM), or one that describes a lowering rather than the values the caller passes (`Fast`,
        // `C`). Each of those is still callable with raw field elements. Keep the first one and go
        // on looking: the manifest is free to write the Component Model export after it, and that
        // one is worth more.
        untyped.get_or_insert(proc);
    }

    untyped.ok_or_else(|| {
        CliError::InvalidArgument(format!(
            "Procedure '{procedure_name}' not found. Available:\n{}",
            available.join("\n")
        ))
    })
}

/// Parses `args` as one field element each, for a procedure whose types the package does not
/// describe.
///
/// A value is written the way a `felt` is written on the typed path: in decimal. The untyped path
/// is the fallback, so accepting more than the typed one would teach a syntax that stops working as
/// soon as the procedure gains a signature.
fn encode_raw_args(args: &[String]) -> Result<Vec<Felt>, CliError> {
    args.iter()
        .map(|arg| {
            let value: u64 = arg.parse().map_err(|_| {
                CliError::InvalidArgument(format!("Invalid argument '{arg}'. Expected a felt."))
            })?;
            Felt::try_from(value).map_err(|_| {
                CliError::InvalidArgument(format!("Argument '{arg}' is too large for a felt."))
            })
        })
        .collect()
}

/// Builds a transaction script that pushes `args` and calls the procedure at `digest`.
///
/// Only the top results are read back, and `truncate_stack` restores the 16-element exit invariant,
/// so anything left below the results can stay there.
pub(crate) fn generate_tx_script(
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

// LOCAL ACCOUNT
// ================================================================================================

/// How the client tracks an account. It decides whether a call runs on the local account, runs
/// against a copy read from the network, or cannot run.
pub(crate) enum LocalAccount {
    /// The account is tracked locally and its local state matches the network.
    Usable,
    /// The account is tracked locally, but its local state does not match the network.
    Locked,
    /// The account is not tracked locally.
    Untracked,
}

/// Reads how the client tracks `target_id`.
///
/// Both `call` and `send` decide on the tracking state, so the check for a locked account stays
/// here. Each command keeps its own error message and control flow.
pub(crate) async fn classify_local_account<AUTH: Keystore + Sync + 'static>(
    client: &Client<AUTH>,
    target_id: AccountId,
) -> Result<LocalAccount, CliError> {
    match client.get_account_header(target_id).await? {
        Some((_, status)) if status.is_locked() => Ok(LocalAccount::Locked),
        Some(_) => Ok(LocalAccount::Usable),
        None => Ok(LocalAccount::Untracked),
    }
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
        // The last part of the lowered path holds the whole interface, so it never equals the plain
        // name. Were it found, its `i32` return would be printed as a value.
        let manifest = manifest_with_exports(&[lowered_form()]);

        // The message itself is pinned by `an_unknown_procedure_lists_the_whole_export_surface`.
        assert!(resolve_procedure_export(&manifest, "increment-by").is_err());
    }

    #[test]
    fn an_underscore_query_finds_a_kebab_export() {
        let manifest = manifest_with_exports(&[interface_form(), lowered_form()]);

        let export = resolve_procedure_export(&manifest, "increment_by").unwrap();
        assert_eq!(export.signature, interface_form().1);
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
    fn an_untyped_export_is_resolved_but_never_shadows_the_component_model_one() {
        // Every export that is not the Component Model one takes the fallback path, whether it
        // carries no signature at all (hand-written MASM) or one describing a lowering. Each is
        // still callable with raw field elements, so it resolves on its own, and each must lose to
        // the Component Model export in whichever order the manifest writes the two.
        let untyped_forms = [
            ("::mix::\"increment-by\"", None),
            ("::mix::increment_by", Some(FunctionType::new(CallConv::Fast, [], [Type::U32]))),
        ];

        for untyped in untyped_forms {
            let manifest = manifest_with_exports(slice::from_ref(&untyped));
            let export = resolve_procedure_export(&manifest, "increment-by").unwrap();
            assert_eq!(export.signature, untyped.1, "{} did not resolve alone", untyped.0);

            for exports in
                [[untyped.clone(), interface_form()], [interface_form(), untyped.clone()]]
            {
                let manifest = manifest_with_exports(&exports);
                let export = resolve_procedure_export(&manifest, "increment-by").unwrap();
                assert_eq!(export.signature, interface_form().1, "{} won", untyped.0);
            }
        }
    }

    /// The Goldilocks field modulus, `2^64 - 2^32 + 1`. The first value with no felt of its own.
    const FIELD_MODULUS: u64 = 18_446_744_069_414_584_321;

    #[test]
    fn raw_arguments_are_read_as_decimal_felts() {
        let args = ["0", "10", (FIELD_MODULUS - 1).to_string().as_str()].map(String::from);

        let encoded = encode_raw_args(&args).unwrap();

        let expected = [0, 10, FIELD_MODULUS - 1].map(|v| Felt::new(v).unwrap());
        assert_eq!(encoded, expected);
    }

    #[test]
    fn a_raw_argument_that_is_not_a_decimal_felt_is_rejected() {
        let cases = [
            // What an unchecked `u64` argument would silently wrap around to.
            (
                FIELD_MODULUS.to_string(),
                format!("invalid argument: Argument '{FIELD_MODULUS}' is too large for a felt."),
            ),
            // The typed path writes a `felt` in decimal and reserves `0x` for wider values, so the
            // untyped path cannot take hex either: it would work only until the procedure is given
            // a signature.
            (
                "0xff".to_string(),
                "invalid argument: Invalid argument '0xff'. Expected a felt.".to_string(),
            ),
        ];

        for (arg, expected) in cases {
            let err = encode_raw_args(slice::from_ref(&arg)).unwrap_err();

            assert_eq!(err.to_string(), expected, "argument '{arg}'");
        }
    }
}

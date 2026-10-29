//! Renders the call tree of a replayed run, filtered by `--verbose`.

use std::fmt::Write as _;

use miden_client::Felt;
use miden_client::assembly::Path;
use miden_client::transaction::trace::{CallFrameRecord, CallTraceReplay, TracedArg};
use miden_client::vm::typed::TypedProcInfo;
use miden_client::vm::{MIN_STACK_DEPTH, Package, PackageExport};

use super::EntryCall;

/// The result for the entry frame: the decoded value, or the felts the procedure returned when they
/// are not a valid value of its return type. The entry frame itself only holds the C ABI result,
/// which is a pointer for a value wider than one felt.
pub fn entry_result(
    output_stack: &[Felt; MIN_STACK_DEPTH],
    typed: &TypedProcInfo,
) -> Option<String> {
    if let Ok(rendered) = typed.decode_result(output_stack.as_slice()) {
        return rendered;
    }

    let count = typed.output_felt_count()?;
    let felts = output_stack[..count].iter().map(Felt::as_canonical_u64).collect::<Vec<_>>();
    Some(format_felts(&felts))
}

// CALL TREE
// ================================================================================================

/// Tells if a frame runs code of the called package, or other code, such as intrinsics, kernel
/// procedures, or hand-written MASM.
#[derive(Default)]
struct PackageScope<'a> {
    /// The modules the package exports from. The package's procedures sit below them.
    roots: Vec<&'a Path>,
}

impl<'a> PackageScope<'a> {
    fn new(package: &'a Package) -> Self {
        let mut roots: Vec<&Path> = Vec::new();
        for export in package.manifest.exports() {
            if let PackageExport::Procedure(proc) = export
                && let Some(root) = proc.path.parent()
                && !roots.contains(&root)
            {
                roots.push(root);
            }
        }
        Self { roots }
    }

    /// Whether `path` names a procedure below one of the export modules.
    fn contains(&self, path: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| path.strip_prefix(root).is_some_and(|rest| !rest.is_empty()))
    }
}

/// Whether `callee` is the export named `name`. The export body runs under the export name, while
/// the contract's own procedures carry mangled Rust symbols.
fn is_entry_frame(callee: &str, name: &str, scope: &PackageScope<'_>) -> bool {
    let callee = Path::new(callee);
    scope.contains(callee) && callee.last().is_some_and(|last| is_same_export(last, name))
}

/// Whether a frame name and a manifest export name are the same export. The two can spell it
/// differently: `get_count`, the WIT form `get-count`, or the component export
/// `miden:counter/counter@0.1.0#get-count` that older compilers give the export body.
fn is_same_export(frame: &str, export: &str) -> bool {
    let frame = frame.rsplit_once('#').map_or(frame, |(_, name)| name);
    frame.replace('-', "_") == export.replace('-', "_")
}

/// What kind of code a frame ran. `--verbose` filters on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FrameKind {
    /// Rust code the contract's crate defines.
    User,
    /// Rust code from another crate in the package, such as the Rust SDK, and the bindings that the
    /// SDK generates for the contract.
    Library,
    /// Procedures that are not Rust code, such as hand-written MASM.
    Internal,
}

impl FrameKind {
    /// The last kind that is still shown at a `--verbose` level.
    fn shown_at(verbose: u8) -> Self {
        match verbose {
            0 => Self::User,
            1 => Self::Library,
            _ => Self::Internal,
        }
    }
}

/// Renders the call tree of the replayed run, to print under the result of the real run.
/// `entry_result` is the decoded result of the called procedure.
pub(super) fn render_call_trace(
    replay: Option<&CallTraceReplay>,
    package: Option<&Package>,
    entry: Option<&EntryCall>,
    entry_result: Option<&str>,
    verbose: u8,
) -> String {
    let Some(replay) = replay else {
        return "\nTrace: the execution left no recording.\n".to_string();
    };

    let mut out = String::new();
    writeln!(out, "\nReplay: {} cycles", replay.cycles).unwrap();
    if let Some(err) = &replay.error {
        writeln!(
            out,
            "Warning: the replay stopped with an error; the trace below is incomplete: {err}"
        )
        .unwrap();
    }

    let max_kind = FrameKind::shown_at(verbose);

    let scope = package.map(PackageScope::new).unwrap_or_default();
    let mut printer = Printer {
        out,
        entry,
        entry_result,
        scope: &scope,
        max_kind,
        shown: 0,
        hidden: 0,
    };
    writeln!(printer.out, "\nCall trace:").unwrap();
    printer.print(&replay.trace.roots, 0);

    if printer.shown == 0 {
        writeln!(printer.out, "  (no frames to show)").unwrap();
    }
    if printer.hidden > 0 {
        let more = match max_kind {
            FrameKind::User => "library functions with -v, everything with -vv",
            _ => "everything with -vv",
        };
        writeln!(printer.out, "\n{} frame(s) hidden; show {more}.", printer.hidden).unwrap();
    }
    printer.out
}

/// Walks the tree. A frame that is not printed is replaced by its children, so the nesting stays
/// right.
struct Printer<'a> {
    /// The lines printed so far.
    out: String,
    /// The call the user made, which names and renders the entry frame.
    entry: Option<&'a EntryCall>,
    /// The decoded result of that call, if there is one.
    entry_result: Option<&'a str>,
    /// Which frames are the package's own code.
    scope: &'a PackageScope<'a>,
    /// The last kind that is still printed.
    max_kind: FrameKind,
    shown: usize,
    /// Frames that a higher `--verbose` level shows. Frames without a name are not counted, since
    /// no level shows them.
    hidden: usize,
}

impl Printer<'_> {
    fn print(&mut self, frames: &[CallFrameRecord], depth: usize) {
        for frame in frames {
            // The procedure the user called is always shown, whatever its path looks like.
            let entry = self.entry.filter(|entry| {
                frame
                    .callee
                    .as_deref()
                    .is_some_and(|callee| is_entry_frame(callee, &entry.name, self.scope))
            });
            let name = frame.callee.as_deref().and_then(|callee| shorten(callee, self.scope));

            let (label, kind) = match (entry, name) {
                (Some(entry), _) => (entry.name.clone(), FrameKind::User),
                (None, Some(name)) => (name.text, name.kind),
                (None, None) => {
                    self.print(&frame.children, depth);
                    continue;
                },
            };
            if kind > self.max_kind {
                self.hidden += 1;
                self.print(&frame.children, depth);
                continue;
            }

            self.shown += 1;
            let indent = "  ".repeat(depth + 1);
            let args = match entry {
                Some(entry) => entry.args.clone(),
                None => format_args_list(&frame.args),
            };
            // A result that could not be decoded falls back to the felts the frame returned.
            let results = match entry.and(self.entry_result) {
                Some(result) => format!(" -> {result}"),
                None => format_results(frame.results.as_deref()),
            };
            // Every frame that was open when the run stopped has no exit cycle. The innermost one,
            // the only open frame without an open child, is where execution stopped.
            let cycles = match frame.cycles() {
                Some(cycles) => format!("{cycles} cycles"),
                None if frame.children.iter().any(|child| child.exit_clk.is_none()) => {
                    "did not return".to_string()
                },
                None => "stopped here".to_string(),
            };
            writeln!(self.out, "{indent}{label}({args}){results}  {cycles}").unwrap();

            self.print(&frame.children, depth + 1);
        }
    }
}

/// A procedure path cut down for printing, and the kind of code it names.
struct ShortName {
    text: String,
    kind: FrameKind,
}

/// Cuts a procedure path down to its Rust path and tells whose code it is. Returns `None` for a
/// name that does not name the callee.
///
/// Every traced frame is a procedure call, so its name is an absolute path. A relative name, such
/// as `<core::result::Result<T, E>>::unwrap`, is an inline function of the caller that the debug
/// engine took as the callee. Such a name is wrong, so the frame is treated as having no name.
///
/// Only a mangled Rust symbol is Rust code. Its demangled path starts with the crate that defines
/// it. A procedure of the package has the path `<export module>::<crate>::<symbol>`, where
/// `<crate>` is the crate the package was compiled from. Everything else, such as
/// `::intrinsics::mem::load_sw`, is internal.
fn shorten(name: &str, scope: &PackageScope<'_>) -> Option<ShortName> {
    if !name.starts_with("::") {
        return None;
    }
    let path = Path::new(name);
    let rust = path
        .split_last()
        .filter(|_| scope.contains(path))
        .and_then(|(symbol, module)| Some((rustc_demangle::try_demangle(symbol).ok()?, module)));
    let Some((demangled, module)) = rust else {
        return Some(ShortName {
            kind: FrameKind::Internal,
            text: path.to_relative().as_str().to_string(),
        });
    };
    let rust_path = format!("{demangled:#}");

    // Own code starts with the crate, directly or as the type of a trait method.
    let own = module.last().filter(|own_crate| {
        (rust_path.starts_with(&format!("{own_crate}::"))
            || rust_path.starts_with(&format!("<{own_crate}::")))
            && !rust_path.contains(&format!("{own_crate}::bindings::"))
    });

    // `<T as Trait>::m` and the inherent method `<T>::m` become `T::m`. The `>` that closes the
    // qualifier is the first one outside the generic arguments of `T`.
    let without_qualifier = rust_path.strip_prefix('<').and_then(|rest| {
        let mut depth = 0usize;
        let close = rest.char_indices().find_map(|(index, c)| match c {
            '<' => {
                depth += 1;
                None
            },
            '>' if depth == 0 => Some(index),
            '>' => {
                depth -= 1;
                None
            },
            _ => None,
        })?;
        let method = rest[close + 1..].strip_prefix("::")?;
        let qualifier = &rest[..close];
        let ty = qualifier.split_once(" as ").map_or(qualifier, |(ty, _)| ty);
        Some(format!("{ty}::{method}"))
    });
    let text = without_qualifier.as_deref().unwrap_or(&rust_path);

    // The own crate prefix is dropped, since it is the same on every line.
    let text = own
        .and_then(|own_crate| text.strip_prefix(&format!("{own_crate}::")))
        .unwrap_or(text);

    Some(ShortName {
        kind: if own.is_some() {
            FrameKind::User
        } else {
            FrameKind::Library
        },
        text: text.to_string(),
    })
}

/// Prints the arguments the way a call site writes them. An unreadable value is shown as `?`.
///
/// The `self` argument is left out. Its value is a memory address, which tells the user nothing.
fn format_args_list(args: &[TracedArg]) -> String {
    args.iter()
        .filter(|arg| arg.name != "self")
        .map(|arg| match arg.values.as_deref() {
            Some(values) => format!("{}={}", arg.name, format_felts(values)),
            None => format!("{}=?", arg.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Prints one felt on its own, and more than one as a list.
fn format_felts(felts: &[u64]) -> String {
    match felts {
        [single] => single.to_string(),
        many => {
            format!("[{}]", many.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        },
    }
}

/// Prints what a call returned: nothing for no value, and `?` for a value that could not be read.
fn format_results(results: Option<&[u64]>) -> String {
    match results {
        None => " -> ?".to_string(),
        Some([]) => String::new(),
        Some(values) => format!(" -> {}", format_felts(values)),
    }
}

#[cfg(test)]
mod tests {
    use midenc_hir_type::{CallConv, FunctionType, Type};

    use super::*;

    /// The module the counter contract exports from.
    const ROOT: &str = "::miden::counter_contract::counter_contract";

    /// A procedure of the contract's crate, which sits one module below the export module.
    fn crate_proc(symbol: &str) -> String {
        format!("{ROOT}::counter_contract::{symbol}")
    }

    fn counter_scope() -> PackageScope<'static> {
        PackageScope { roots: vec![Path::new(ROOT)] }
    }

    fn shorten_in_package(path: &str) -> ShortName {
        shorten(path, &counter_scope()).expect("an absolute path has a name")
    }

    /// A finished frame that took one cycle and has no arguments and no result.
    fn frame(callee: &str, children: Vec<CallFrameRecord>) -> CallFrameRecord {
        CallFrameRecord {
            callee: Some(callee.to_string()),
            ..unnamed_frame(children)
        }
    }

    /// A finished frame that the debug engine could not name.
    fn unnamed_frame(children: Vec<CallFrameRecord>) -> CallFrameRecord {
        CallFrameRecord {
            callee: None,
            enter_clk: 0,
            exit_clk: Some(1),
            args: Vec::new(),
            results: Some(Vec::new()),
            children,
        }
    }

    /// The lines a printer writes for `roots` at the default level, with `entry` as the call.
    fn printed(roots: &[CallFrameRecord], entry: Option<&EntryCall>) -> String {
        printed_at(roots, entry, FrameKind::User).0
    }

    /// The lines a printer writes for `roots` when it shows up to `max_kind`, and the number of
    /// frames it counts as hidden.
    fn printed_at(
        roots: &[CallFrameRecord],
        entry: Option<&EntryCall>,
        max_kind: FrameKind,
    ) -> (String, usize) {
        let scope = counter_scope();
        let mut printer = Printer {
            out: String::new(),
            entry,
            entry_result: Some("10"),
            scope: &scope,
            max_kind,
            shown: 0,
            hidden: 0,
        };
        printer.print(roots, 0);
        (printer.out, printer.hidden)
    }

    #[test]
    fn own_crate_symbols_are_demangled_trimmed_and_recognised() {
        let name = shorten_in_package(&crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level1"));
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "level1");

        let name = shorten_in_package(&crate_proc(
            "_RNvXsa_Cs1gvHueIcLs7_16counter_contractNtB5_22CounterContractStorageNtB5_15CounterContract11depth_probe",
        ));
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "CounterContractStorage::depth_probe");
    }

    #[test]
    fn inherent_methods_drop_the_angle_brackets() {
        // `<portfolio::PortfolioStorage>::quote`, a procedure of the `portfolio` crate.
        let name = shorten_in_package(&format!(
            "{ROOT}::portfolio::_RNvMCs5DqNazyoeqA_9portfolioNtB2_16PortfolioStorage5quote"
        ));
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "PortfolioStorage::quote");
    }

    #[test]
    fn quoted_symbols_are_demangled_and_trimmed() {
        // The assembler quotes a symbol that carries an `.llvm.<hash>` suffix.
        let name = shorten_in_package(&crate_proc(
            "\"_RNvCs1gvHueIcLs7_16counter_contract6level1.llvm.8598756201189951195\"",
        ));
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "level1");

        // A name that is not a mangled symbol is not taken for Rust code, even if it reads like a
        // Rust path.
        let name = shorten_in_package(&crate_proc(
            "\"<counter_contract::Counter as miden::Component>::get\"",
        ));
        assert_eq!(name.kind, FrameKind::Internal);
    }

    #[test]
    fn other_crates_are_library_and_paths_outside_the_package_are_internal() {
        let name =
            shorten_in_package(&crate_proc("_RNvNtCs4mQadDl6yut_11wit_bindgen2rt14run_ctors_once"));
        assert_eq!(name.kind, FrameKind::Library);
        assert_eq!(name.text, "wit_bindgen::rt::run_ctors_once");

        let name = shorten_in_package("::intrinsics::mem::load_sw");
        assert_eq!(name.kind, FrameKind::Internal);
        assert_eq!(name.text, "intrinsics::mem::load_sw");

        // It shares the `miden` namespace with the contract, but it is not under the export module.
        let name = shorten_in_package("::miden::core::math::u64::gt");
        assert_eq!(name.kind, FrameKind::Internal);
        assert_eq!(name.text, "miden::core::math::u64::gt");

        // Without a package nothing is the contract's code.
        let name = shorten(
            &crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level1"),
            &PackageScope::default(),
        )
        .expect("an absolute path has a name");
        assert_eq!(name.kind, FrameKind::Internal);
    }

    #[test]
    fn frames_named_after_an_inline_function_have_no_name() {
        // The debug engine names a frame after an inline function of the caller by its relative
        // Rust path. The name never belongs to the callee, own crate or not.
        for name in [
            "<core::result::Result<miden_field::wasm_miden::Felt, miden_field::wasm_miden::FeltFromIntError>>::unwrap",
            "counter_contract::tax_of",
            "<counter_contract::CounterContractStorage as counter_contract::CounterContract>::credit_deposit",
        ] {
            assert!(shorten(name, &counter_scope()).is_none(), "{name}");
        }
    }

    #[test]
    fn frames_without_a_name_are_left_out_at_every_level() {
        let roots = [unnamed_frame(vec![
            frame("::intrinsics::mem::load_sw", Vec::new()),
            frame(
                "<core::result::Result<miden_field::wasm_miden::Felt, miden_field::wasm_miden::FeltFromIntError>>::unwrap",
                vec![frame(&crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level2"), Vec::new())],
            ),
        ])];

        // Their children take their place, and they are not counted as hidden.
        assert_eq!(
            printed_at(&roots, None, FrameKind::Internal),
            ("  intrinsics::mem::load_sw()  1 cycles\n  level2()  1 cycles\n".to_string(), 0)
        );
        assert_eq!(
            printed_at(&roots, None, FrameKind::User),
            ("  level2()  1 cycles\n".to_string(), 1)
        );
    }

    #[test]
    fn a_hidden_frame_leaves_its_children_in_place() {
        let roots = [frame(
            &crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level1"),
            vec![frame(
                "<core::result::Result<miden_field::wasm_miden::Felt, miden_field::wasm_miden::FeltFromIntError>>::unwrap",
                vec![frame(&crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level2"), Vec::new())],
            )],
        )];

        assert_eq!(printed(&roots, None), "  level1()  1 cycles\n    level2()  1 cycles\n");
    }

    #[test]
    fn the_called_export_stays_visible_and_prints_the_call_as_typed() {
        let roots = [frame(
            &crate_proc("depth_probe"),
            vec![frame(&crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level1"), Vec::new())],
        )];
        let entry = EntryCall {
            name: "depth_probe".to_string(),
            args: "3, 7".to_string(),
        };

        // The export body runs under its plain name, which no symbol of the own crate has, so only
        // the entry tells it from library code.
        assert_eq!(
            printed(&roots, Some(&entry)),
            "  depth_probe(3, 7) -> 10  1 cycles\n    level1()  1 cycles\n"
        );
        assert_eq!(printed(&roots, None), "  level1()  1 cycles\n");
    }

    #[test]
    fn the_entry_frame_shows_the_returned_felts_when_they_do_not_decode() {
        let typed_returning = |result: Type| {
            TypedProcInfo::new("probe", FunctionType::new(CallConv::ComponentModel, [], [result]))
                .unwrap()
        };
        let stack_with = |top: &[u64]| {
            let mut stack = [Felt::new(0).unwrap(); MIN_STACK_DEPTH];
            for (slot, value) in stack.iter_mut().zip(top) {
                *slot = Felt::new(*value).unwrap();
            }
            stack
        };
        let too_wide_for_u32 = 1u64 << 40;

        let cases = [
            (Type::U32, vec![7], Some("7u32")),
            (Type::U32, vec![too_wide_for_u32], Some("1099511627776")),
            (Type::U64, vec![too_wide_for_u32, 0], Some("[1099511627776, 0]")),
        ];

        for (result, top, expected) in cases {
            let rendered = entry_result(&stack_with(&top), &typed_returning(result.clone()));
            assert_eq!(rendered.as_deref(), expected, "{result} from {top:?}");
        }
    }

    #[test]
    fn generated_bindings_are_library_code_although_they_name_the_contracts_crate() {
        // The `Guest` impl that the `#[component]` macro writes for the contract.
        let name = shorten_in_package(&crate_proc(
            "_RNvXsb_Cs1gvHueIcLs7_16counter_contractNtB5_22CounterContractStorageNtNtNtNtNtNtB5_8bindings7exports5miden16counter_contract16counter_contract5Guest11depth_probe",
        ));
        assert_eq!(name.kind, FrameKind::Library);
        assert_eq!(name.text, "counter_contract::CounterContractStorage::depth_probe");
    }

    #[test]
    fn the_called_export_is_told_apart_from_the_contracts_own_procedures() {
        let scope = counter_scope();
        let export = crate_proc("depth_probe");
        assert!(is_entry_frame(&export, "depth_probe", &scope));
        assert!(!is_entry_frame(&export, "apply_op", &scope));

        // The Rust function behind the export is a procedure of its own, and is not the entry.
        assert!(!is_entry_frame(
            &crate_proc(
                "_RNvXsa_Cs1gvHueIcLs7_16counter_contractNtB5_22CounterContractStorageNtB5_15CounterContract11depth_probe"
            ),
            "depth_probe",
            &scope
        ));

        // A library procedure with the same name is not the entry.
        assert!(!is_entry_frame("::miden::core::math::u64::gt", "gt", &scope));
    }

    #[test]
    fn the_export_is_recognised_under_every_spelling_of_its_name() {
        let scope = counter_scope();
        for frame in [
            "depth_probe",
            "depth-probe",
            "\"miden:counter-contract/counter-contract@0.1.0#depth-probe\"",
        ] {
            let callee = crate_proc(frame);
            assert!(is_entry_frame(&callee, "depth_probe", &scope), "{frame}");
            assert!(is_entry_frame(&callee, "depth-probe", &scope), "{frame}");
            assert!(!is_entry_frame(&callee, "depth", &scope), "{frame}");
        }
    }

    #[test]
    fn verbose_levels_widen_what_is_shown() {
        assert_eq!(FrameKind::shown_at(0), FrameKind::User);
        assert_eq!(FrameKind::shown_at(1), FrameKind::Library);
        assert_eq!(FrameKind::shown_at(2), FrameKind::Internal);
        assert_eq!(FrameKind::shown_at(7), FrameKind::Internal);
    }

    #[test]
    fn results_and_args_render_compactly() {
        assert_eq!(format_results(None), " -> ?");
        assert_eq!(format_results(Some(&[])), "");
        assert_eq!(format_results(Some(&[7])), " -> 7");
        assert_eq!(format_results(Some(&[1, 2])), " -> [1, 2]");
        let args = [
            TracedArg {
                index: 0,
                name: "self".into(),
                felt_count: Some(1),
                values: Some(vec![1_048_560]),
            },
            TracedArg {
                index: 1,
                name: "op".into(),
                felt_count: Some(1),
                values: Some(vec![0]),
            },
            TracedArg {
                index: 2,
                name: "amount".into(),
                felt_count: Some(2),
                values: Some(vec![1, 2]),
            },
            TracedArg {
                index: 3,
                name: "x".into(),
                felt_count: None,
                values: None,
            },
        ];
        assert_eq!(format_args_list(&args), "op=0, amount=[1, 2], x=?");
    }
}

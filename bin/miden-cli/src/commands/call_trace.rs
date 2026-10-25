//! Prints the call tree that `call --trace` records, filtered by `--verbose`.

use miden_client::assembly::Path;
use miden_client::transaction::trace::{CallFrameRecord, CallTraceReplay, TracedArg};
use miden_client::vm::{Package, PackageExport};

/// The call as typed. The entry frame prints from this, since the frame itself only carries the C
/// ABI form: flattened felts in, a pointer out.
pub struct EntryCall<'a> {
    /// The export name, such as `move-point`.
    pub name: &'a str,
    /// The arguments as the caller wrote them, already rendered.
    pub args: &'a str,
    /// The decoded result, or the returned felts when they do not decode. `None` when the run
    /// produced no result.
    pub result: Option<String>,
}

/// What the package tells about which frames are its own code.
#[derive(Default)]
struct PackageScope<'a> {
    /// The modules the package exports from. The package's procedures sit below them.
    roots: Vec<&'a Path>,
}

impl<'a> PackageScope<'a> {
    fn new(package: &'a Package) -> Self {
        let roots = package
            .manifest
            .exports()
            .filter_map(|export| match export {
                PackageExport::Procedure(proc) => proc.path.parent(),
                _ => None,
            })
            .collect();
        Self { roots }
    }

    /// Whether `path` names a procedure below one of the export modules.
    fn contains(&self, path: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| path.strip_prefix(root).is_some_and(|rest| !rest.is_empty()))
    }
}

/// Whether `callee` is the export named `name`. The export body runs under the plain export name,
/// while the contract's own procedures carry mangled Rust symbols.
fn is_entry_frame(callee: &str, name: &str, scope: &PackageScope<'_>) -> bool {
    let callee = Path::new(callee);
    scope.contains(callee) && callee.last() == Some(name)
}

/// What kind of code a frame ran. `--verbose` filters on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FrameKind {
    /// A procedure from the contract's own crate.
    User,
    /// A procedure from another crate in the package: the Rust SDK and its dependencies.
    Library,
    /// Everything else: hand-written MASM intrinsics, frames with no name, and frames named after
    /// an inline function.
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

/// Prints the call tree of the replayed run, under the result of the real run.
pub fn print_call_trace(
    replay: Option<&CallTraceReplay>,
    package: Option<&Package>,
    entry: Option<&EntryCall<'_>>,
    verbose: u8,
) {
    let Some(replay) = replay else {
        println!("\nTrace: the execution left no recording.");
        return;
    };

    println!("\nReplay: {} cycles", replay.cycles);
    if let Some(err) = &replay.error {
        println!("Warning: the replay stopped with an error; the trace below is incomplete: {err}");
    }

    // Without a package no frame has a name, so there is nothing to filter on.
    let max_kind = match package {
        Some(_) => FrameKind::shown_at(verbose),
        None => FrameKind::Internal,
    };

    let scope = package.map(PackageScope::new).unwrap_or_default();
    let mut printer = Printer {
        entry,
        scope: &scope,
        max_kind,
        shown: 0,
        hidden: 0,
    };
    println!("\nCall trace:");
    printer.print(&replay.trace.roots, 0);

    if printer.shown == 0 {
        println!("  (no frames to show)");
    }
    if printer.hidden > 0 {
        let more = match max_kind {
            FrameKind::User => "library functions with -v, everything with -vv",
            _ => "everything with -vv",
        };
        println!("\n{} frame(s) hidden; show {more}.", printer.hidden);
    }
}

/// Walks the tree. A hidden frame is replaced by its children, so the nesting stays right.
struct Printer<'a> {
    /// The call the user made, which names and renders the entry frame.
    entry: Option<&'a EntryCall<'a>>,
    /// Which frames are the package's own code.
    scope: &'a PackageScope<'a>,
    /// The last kind that is still printed.
    max_kind: FrameKind,
    shown: usize,
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
                    .is_some_and(|callee| is_entry_frame(callee, entry.name, self.scope))
            });
            let name = frame.callee.as_deref().map(|callee| shorten(callee, self.scope));

            let kind = match entry {
                Some(_) => FrameKind::User,
                None => name.as_ref().map_or(FrameKind::Internal, |name| name.kind),
            };
            if kind > self.max_kind {
                self.hidden += 1;
                self.print(&frame.children, depth);
                continue;
            }

            self.shown += 1;
            let indent = "  ".repeat(depth + 1);
            let label = match entry {
                Some(entry) => entry.name.to_string(),
                None => name.map_or_else(|| "<unknown>".to_string(), |name| name.text),
            };
            let args = match entry {
                Some(entry) => entry.args.to_string(),
                None => format_args_list(&frame.args),
            };
            // A result that could not be decoded falls back to the felts the frame returned.
            let results = match entry.and_then(|entry| entry.result.as_deref()) {
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
            println!("{indent}{label}({args}){results}  {cycles}");

            self.print(&frame.children, depth + 1);
        }
    }
}

/// A procedure path cut down for printing, and the kind of code it names.
struct ShortName {
    text: String,
    kind: FrameKind,
}

/// Cuts a procedure path down to its Rust path and tells whose code it is.
///
/// A procedure of the package has the path `<export module>::<crate>::<symbol>`. A Rust path under
/// the own crate is user code, except the generated `bindings` module. A path outside the package,
/// such as `::intrinsics::mem::load_sw`, is MASM.
///
/// Every traced frame is a procedure call, so its name is an absolute path. A relative name, such
/// as `<core::result::Result<T, E>>::unwrap`, is an inline function of the caller that the debug
/// engine took as the callee. Such a name is wrong, so the frame is internal.
fn shorten(name: &str, scope: &PackageScope<'_>) -> ShortName {
    let path = Path::new(name);
    let procedure = path.split_last().filter(|_| name.starts_with("::") && scope.contains(path));
    let Some((symbol, module)) = procedure else {
        return ShortName {
            kind: FrameKind::Internal,
            text: path.to_relative().as_str().to_string(),
        };
    };
    let rust_path = demangle(symbol);

    // Own code starts with the crate, directly or as the type of a trait method.
    let own = module.last().filter(|own_crate| {
        (rust_path.starts_with(&format!("{own_crate}::"))
            || rust_path.starts_with(&format!("<{own_crate}::")))
            && !rust_path.contains(&format!("{own_crate}::bindings::"))
    });

    // `<T as Trait>::m` becomes `T::m`.
    let without_trait = rust_path
        .strip_prefix('<')
        .and_then(|rest| rest.split_once(" as "))
        .and_then(|(ty, rest)| rest.split_once(">::").map(|(_, method)| format!("{ty}::{method}")));
    let text = without_trait.as_deref().unwrap_or(&rust_path);

    // The own crate prefix is dropped, since it is the same on every line.
    let text = own
        .and_then(|own_crate| text.strip_prefix(&format!("{own_crate}::")))
        .unwrap_or(text);

    ShortName {
        kind: if own.is_some() {
            FrameKind::User
        } else {
            FrameKind::Library
        },
        text: text.to_string(),
    }
}

/// Demangles a Rust symbol. A name that is not a mangled symbol comes back unchanged.
fn demangle(name: &str) -> String {
    let mut input = name.as_bytes();
    let mut out = Vec::with_capacity(name.len() * 2);
    match rustc_demangle::demangle_stream(&mut input, &mut out, false) {
        Ok(()) => String::from_utf8(out).unwrap_or_else(|_| name.to_string()),
        Err(_) => name.to_string(),
    }
}

/// Prints the arguments the way a call site writes them. An unreadable value is shown as `?`.
fn format_args_list(args: &[TracedArg]) -> String {
    args.iter()
        .map(|arg| match arg.values.as_deref() {
            Some(values) => format!("{}={}", arg.name, format_felts(values)),
            None => format!("{}=?", arg.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Prints one felt on its own, and more than one as a list.
pub(crate) fn format_felts(felts: &[u64]) -> String {
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
        shorten(path, &counter_scope())
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
    fn quoted_symbols_are_demangled_and_trimmed() {
        // The assembler quotes a symbol that carries an `.llvm.<hash>` suffix.
        let name = shorten_in_package(&crate_proc(
            "\"_RNvCs1gvHueIcLs7_16counter_contract6level1.llvm.8598756201189951195\"",
        ));
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "level1");

        // The assembler also quotes a symbol that is not a plain identifier.
        let name = shorten_in_package(&crate_proc(
            "\"<counter_contract::Counter as miden::Component>::get\"",
        ));
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "Counter::get");
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
        );
        assert_eq!(name.kind, FrameKind::Internal);
    }

    #[test]
    fn frames_named_after_an_inline_function_are_internal() {
        // The debug engine names a frame after an inline function of the caller by its relative
        // Rust path. The name never belongs to the callee, own crate or not.
        for name in [
            "<core::result::Result<miden_field::wasm_miden::Felt, miden_field::wasm_miden::FeltFromIntError>>::unwrap",
            "counter_contract::tax_of",
            "<counter_contract::CounterContractStorage as counter_contract::CounterContract>::credit_deposit",
        ] {
            let short = shorten_in_package(name);
            assert_eq!(short.kind, FrameKind::Internal, "{name}");
            assert_eq!(short.text, name);
        }
    }

    #[test]
    fn a_hidden_frame_leaves_its_children_in_place() {
        let frame = |callee: &str, children| CallFrameRecord {
            callee: Some(callee.to_string()),
            enter_clk: 0,
            exit_clk: Some(1),
            args: Vec::new(),
            results: Some(Vec::new()),
            children,
        };
        let roots = [frame(
            &crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level1"),
            vec![frame(
                "<core::result::Result<miden_field::wasm_miden::Felt, miden_field::wasm_miden::FeltFromIntError>>::unwrap",
                vec![frame(&crate_proc("_RNvCs1gvHueIcLs7_16counter_contract6level2"), Vec::new())],
            )],
        )];

        let scope = counter_scope();
        let mut printer = Printer {
            entry: None,
            scope: &scope,
            max_kind: FrameKind::User,
            shown: 0,
            hidden: 0,
        };
        printer.print(&roots, 0);
        assert_eq!((printer.shown, printer.hidden), (2, 1));
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

//! Prints the call tree that `call --trace` records, filtered by `--verbose`.

use miden_client::assembly::Path;
use miden_client::transaction::trace::{CallFrameRecord, CallTraceReplay, TracedArg};
use miden_client::vm::Package;

/// The call as typed. The entry frame prints from this, since the frame itself only carries the C
/// ABI form: flattened felts in, a pointer out.
pub struct EntryCall<'a> {
    /// The export name, such as `move-point`.
    pub name: &'a str,
    /// The arguments as the caller wrote them, already rendered.
    pub args: &'a str,
    /// The decoded result, or `None` when the run produced none that could be rendered.
    pub result: Option<String>,
}

/// Whether `callee` is the export named `name`. An export runs as `<wit-package>#<export-name>`.
fn is_entry_frame(callee: &str, name: &str) -> bool {
    callee.trim_end_matches('"').ends_with(&format!("#{name}"))
}

/// What kind of code a frame ran. `--verbose` filters on this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FrameKind {
    /// A procedure from the contract's own crate.
    User,
    /// A procedure from another crate in the package: the Rust SDK and its dependencies.
    Library,
    /// Everything else: hand-written MASM intrinsics, and frames with no name.
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

    let mut printer = Printer { entry, max_kind, shown: 0, hidden: 0 };
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
                frame.callee.as_deref().is_some_and(|callee| is_entry_frame(callee, entry.name))
            });
            let name = frame.callee.as_deref().map(shorten);

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

/// Cuts `::"<package>"::<crate>::<symbol>` down to the symbol and tells whose code it is. A symbol
/// under its own crate is user code. A path without a package, such as
/// `::intrinsics::mem::load_sw`, is MASM.
fn shorten(path: &str) -> ShortName {
    let path = Path::new(path);

    // A package id is always quoted, since it holds `:` and `/`. A bare first component is MASM.
    let has_package = path
        .components()
        .nth(1)
        .and_then(Result::ok)
        .is_some_and(|first| first.is_quoted());
    let after_package = if has_package {
        path.split_first().and_then(|(_package, rest)| rest.split_first())
    } else {
        None
    };
    let Some((first, rest)) = after_package else {
        return ShortName {
            kind: FrameKind::Internal,
            text: path.to_relative().as_str().to_string(),
        };
    };

    // A procedure straight under the package has no crate segment.
    let (own_crate, symbol) = if rest.is_empty() {
        (None, first)
    } else {
        (Some(first), rest.as_str().trim_matches('"'))
    };
    let rust_path = demangle(symbol);

    // Own code starts with the crate, directly or as the type of a trait method.
    let own = own_crate.filter(|own_crate| {
        rust_path.starts_with(&format!("{own_crate}::"))
            || rust_path.starts_with(&format!("<{own_crate}::"))
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
    use super::*;

    #[test]
    fn own_crate_paths_are_trimmed_and_recognised() {
        let name = shorten("::\"counter-contract\"::counter_contract::counter_contract::apply_op");
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "apply_op");

        // The assembler quotes a symbol that is not a plain identifier.
        let name = shorten(
            "::\"counter-contract\"::counter_contract::\"<counter_contract::Counter as miden::Component>::get\"",
        );
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "Counter::get");
    }

    #[test]
    fn quoted_symbols_are_demangled_and_trimmed() {
        // The assembler quotes a symbol that carries an `.llvm.<hash>` suffix.
        let name = shorten(
            "::\"miden:counter-contract/counter-contract@0.1.0\"::counter_contract::\"_RNvXs8_CsawNGMLSRMxb_16counter_contractNtB5_22CounterContractStorageNtB5_15CounterContract8apply_op.llvm.8598756201189951195\"",
        );
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "CounterContractStorage::apply_op");
    }

    #[test]
    fn sdk_paths_are_library_and_masm_paths_are_internal() {
        let name = shorten("::\"counter-contract\"::counter_contract::\"miden::felt::add\"");
        assert_eq!(name.kind, FrameKind::Library);
        assert_eq!(name.text, "miden::felt::add");

        let name = shorten(
            "::\"counter-contract\"::counter_contract::_RNvNtNtCs1gfRmJqte2s_14miden_base_sys8bindings4note15build_recipient",
        );
        assert_eq!(name.kind, FrameKind::Library);
        assert_eq!(name.text, "miden_base_sys::bindings::note::build_recipient");

        let name = shorten("::intrinsics::mem::load_sw");
        assert_eq!(name.kind, FrameKind::Internal);
        assert_eq!(name.text, "intrinsics::mem::load_sw");
    }

    #[test]
    fn a_procedure_straight_under_the_package_has_no_crate() {
        let name = shorten("::\"miden:counter-contract/counter-contract@0.1.0\"::init");
        assert_eq!(name.kind, FrameKind::Library);
        assert_eq!(name.text, "init");
    }

    #[test]
    fn the_called_export_is_told_apart_from_the_contracts_own_procedures() {
        // The export the user called runs under `<wit-package>#<export-name>`, quoted because the
        // name is not a plain identifier.
        let export = "::\"miden:counter-contract/counter-contract@0.1.0\"::counter_contract::\"miden:counter-contract/counter-contract@0.1.0#move-point\"";
        assert!(is_entry_frame(export, "move-point"));
        assert!(!is_entry_frame(export, "digest-word"));

        // Its body is a procedure of its own, and is not the entry frame.
        assert!(!is_entry_frame(
            "::\"miden:counter-contract/counter-contract@0.1.0\"::counter_contract::transform",
            "move-point"
        ));
    }

    #[test]
    fn export_wrappers_are_library_code_although_they_name_the_contracts_crate() {
        // The wrapper names the contract's crate as the type it calls the procedure on.
        let name = shorten(
            "::\"miden:counter-contract/counter-contract@0.1.0\"::counter_contract::\"bindings::exports::miden::counter_contract::counter_contract::_export_halve_asset_cabi::<counter_contract::CounterContractStorage>\"",
        );
        assert_eq!(name.kind, FrameKind::Library);

        // The same shape without the wrapper name is the contract's code.
        let name = shorten(
            "::\"miden:counter-contract/counter-contract@0.1.0\"::counter_contract::\"<counter_contract::CounterContractStorage as counter_contract::CounterContract>::halve_asset\"",
        );
        assert_eq!(name.kind, FrameKind::User);
        assert_eq!(name.text, "CounterContractStorage::halve_asset");
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

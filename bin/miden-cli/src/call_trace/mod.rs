//! The `call --trace` flags and the call tree that the flag prints.

mod tree;

use clap::Args;
use miden_client::transaction::trace::CallTraceReplay;
use miden_client::vm::Package;
pub use tree::entry_result;

// FLAGS
// ================================================================================================

// The `call` flags for tracing.
#[derive(Debug, Clone, Args)]
pub struct TraceArgs {
    /// Print the tree of procedure calls, with the arguments, result and cycles of each call.
    #[arg(
        long,
        requires = "package",
        long_help = "Print the hierarchy of procedure calls the execution made, with the \
                     arguments, return value and cycle count of each call.\n\n\
                     The execution is recorded and replayed through the debug engine a cycle at \
                     a time, which is considerably slower than a normal call. Needs \
                     `--package`, as the procedure names come from its debug info."
    )]
    trace: bool,

    /// With `--trace`: `-v` also shows library calls, `-vv` shows every named call.
    #[arg(
        short,
        long,
        action = clap::ArgAction::Count,
        requires = "trace",
        long_help = "With `--trace`, show more frames. By default only the contract's own \
                     procedures are shown. `-v` adds library functions. `-vv` shows every frame \
                     that has a name. Frames without a name are never shown."
    )]
    verbose: u8,
}

impl TraceArgs {
    /// The `--verbose` level, or `None` when `--trace` was not given.
    pub fn level(&self) -> Option<u8> {
        self.trace.then_some(self.verbose)
    }
}

// TRACED CALL
// ================================================================================================

/// The call as the user typed it. The entry frame prints from this, since the frame itself only
/// carries the C ABI form: flattened felts in, a pointer out.
struct EntryCall {
    /// The export name, such as `move-point`.
    name: String,
    /// The arguments as the caller wrote them, already rendered.
    args: String,
}

/// What the trace needs to know about the call, beside the replay.
pub struct TraceCall {
    /// The `--verbose` level, or `None` when `--trace` was not given.
    level: Option<u8>,
    /// The package the procedure came from. Its debug info names the frames.
    package: Option<Package>,
    /// The call as typed. `None` when the procedure was given by digest.
    entry: Option<EntryCall>,
}

impl TraceCall {
    /// `entry` is the export name, the arguments as the caller typed them, and the number of
    /// parameters. It is `None` when the package does not describe the types of the procedure. The
    /// entry frame then prints the felts it received, as every other frame does.
    pub fn new(
        level: Option<u8>,
        package: Package,
        entry: Option<(String, &[String], usize)>,
    ) -> Self {
        let entry = entry.map(|(name, tokens, params)| {
            // A struct parameter takes one token for each field. A comma between those tokens would
            // show them as separate parameters.
            let separator = if tokens.len() == params { ", " } else { " " };
            EntryCall { name, args: tokens.join(separator) }
        });
        Self { level, package: Some(package), entry }
    }

    /// A call given by digest. There is no package, so no frame has a name.
    pub fn without_package(level: Option<u8>) -> Self {
        Self { level, package: None, entry: None }
    }

    /// The `--verbose` level, or `None` when `--trace` was not given.
    pub fn level(&self) -> Option<u8> {
        self.level
    }

    /// The package the procedure came from, if the call named one.
    pub fn package(&self) -> Option<&Package> {
        self.package.as_ref()
    }

    /// Renders the call tree of the replayed run, to print under the result of the real run.
    /// `entry_result` is the decoded result of the called procedure.
    pub fn render(&self, replay: Option<&CallTraceReplay>, entry_result: Option<&str>) -> String {
        tree::render_call_trace(
            replay,
            self.package.as_ref(),
            self.entry.as_ref(),
            entry_result,
            self.level.unwrap_or_default(),
        )
    }
}

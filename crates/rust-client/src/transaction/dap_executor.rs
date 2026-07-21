//! Program executor used by the client's DAP debugging path.
//!
//! The transaction executor is generic over the VM program executor. This wrapper selects the
//! debug-aware executor used by
//! [`Client::execute_program_with_dap`](crate::Client::execute_program_with_dap), allowing a DAP
//! client to attach before execution, set breakpoints, step through the transaction script, inspect
//! VM state, and request restart without changing the normal transaction setup.

use miden_processor::advice::AdviceInputs;
use miden_processor::{
    ExecutionError,
    ExecutionOptions,
    ExecutionOutput,
    FutureMaybeSend,
    Host,
    Program,
    StackInputs,
};
use miden_tx::ProgramExecutor;

/// [`ProgramExecutor`] adapter for the DAP debugging path.
///
/// TODO: Restore interactive DAP debugging. `execute` currently returns an error instead of
/// starting a debug session, so `Client::execute_program_with_dap` is non-functional. The adapter
/// is retained only so the `dap` feature keeps compiling and the debug path fails explicitly rather
/// than silently.
///
/// Why it is blocked: the DAP executor in `miden-debug` 0.9 drives execution from an
/// `Arc<Package>` (to expose package-owned source/debug info to the debugger), but
/// [`ProgramExecutor::execute`] only hands this adapter a bare [`Program`], and there is no
/// lossless way to reconstruct the owning package from a `Program` here.
///
/// Required fix (needs an upstream change, pick one):
/// - `miden-debug` exposes a `Program`-based debug executor (an `execute_async` that accepts
///   `&Program` instead of `Arc<Package>`), OR
/// - `miden-tx`'s [`ProgramExecutor::execute`] passes the owning `Arc<Package>` (or its package
///   debug info) alongside the `Program` so this adapter can forward it to `DapExecutor`.
///
/// Once either lands, replace the error stub below with a real `DapExecutor::execute_async(...)`
/// call and re-enable the `debug_mode_outputs_logs`-style coverage removed in the 0.16 bump.
pub struct DapProgramExecutor;

impl ProgramExecutor for DapProgramExecutor {
    fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Self {
        let _ = (stack_inputs, advice_inputs, options);
        Self
    }

    fn execute<H: Host + Send>(
        self,
        program: &Program,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        // TODO: replace this stub with a real `DapExecutor::execute_async(...)` call once upstream
        // exposes a `Program`-based debug executor (see the type-level docs for the required fix).
        let _ = (program, host);
        async {
            Err(ExecutionError::Internal(
                "DAP debugging is not supported with the current miden-debug backend",
            ))
        }
    }
}

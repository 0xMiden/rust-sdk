//! Program executor used by the client's DAP debugging path.
//!
//! The transaction executor is generic over the VM program executor. This wrapper selects the
//! debug-aware executor used by
//! [`Client::execute_program_with_dap`](crate::Client::execute_program_with_dap), allowing a DAP
//! client to attach before execution, set breakpoints, step through the transaction script, inspect
//! VM state, and request restart without changing the normal transaction setup.

use std::sync::Arc;

use miden_processor::advice::{AdviceError, AdviceInputs};
use miden_processor::{
    ExecutionError,
    ExecutionOptions,
    ExecutionOutput,
    FutureMaybeSend,
    Host,
    Program,
    StackInputs,
};
use miden_protocol::vm::{DebugSourceNodeId, Package, PackageDebugInfo};
use miden_tx::ProgramExecutor;

use super::package::build_debug_package;

/// [`ProgramExecutor`] adapter for [`miden_debug::DapExecutor`].
///
/// The debug information reaches the executor before execution. The package the debugger consumes
/// can only be built once the program is known. Both configured values are therefore held here
/// until [`ProgramExecutor::execute`] assembles the package.
pub struct DapProgramExecutor {
    executor: miden_debug::DapExecutor,
    package_debug_info: PackageDebugInfo,
    entrypoint_source_node: Option<DebugSourceNodeId>,
}

impl DapProgramExecutor {
    fn execute_package<H: Host + Send>(
        self,
        package: Result<Arc<Package>, ExecutionError>,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        async move {
            let package = package?;
            self.executor.execute_async(package, host).await
        }
    }
}

impl ProgramExecutor for DapProgramExecutor {
    fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Result<Self, AdviceError> {
        Ok(Self {
            executor: miden_debug::DapExecutor::new(stack_inputs, advice_inputs, options),
            package_debug_info: PackageDebugInfo::default(),
            entrypoint_source_node: None,
        })
    }

    fn with_debug_info(mut self, package_debug_info: PackageDebugInfo) -> Self {
        self.package_debug_info = package_debug_info;
        self
    }

    fn with_entrypoint_source_node(
        mut self,
        entrypoint_source_node: Option<DebugSourceNodeId>,
    ) -> Self {
        self.entrypoint_source_node = entrypoint_source_node;
        self
    }

    fn execute<H: Host + Send>(
        self,
        program: &Program,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        let package =
            build_debug_package(program, &self.package_debug_info, self.entrypoint_source_node);
        self.execute_package(package, host)
    }
}

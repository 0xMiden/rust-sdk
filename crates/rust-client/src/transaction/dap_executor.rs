//! Program executor used by the client's DAP debugging path.
//!
//! The transaction executor is generic over the VM program executor. This wrapper selects the
//! debug-aware executor used by
//! [`Client::execute_program_with_dap`](crate::Client::execute_program_with_dap), allowing a DAP
//! client to attach before execution, set breakpoints, step through the transaction script, inspect
//! VM state, and request restart without changing the normal transaction setup.

use std::string::{String, ToString};
use std::sync::Arc;

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
use miden_protocol::assembly::{Path, ProcedureName};
use miden_protocol::transaction::TransactionKernel;
use miden_protocol::utils::serde::Serializable;
use miden_protocol::vm::{
    DebugSourceNodeId,
    Package,
    PackageDebugInfo,
    PackageExport,
    ProcedureExport,
    Section,
    SectionId,
    TargetType,
};
use miden_tx::ProgramExecutor;

/// [`ProgramExecutor`] adapter for [`miden_debug::DapExecutor`].
pub struct DapProgramExecutor(miden_debug::DapExecutor);

impl DapProgramExecutor {
    fn execute_package<H: Host + Send>(
        self,
        package: Result<Arc<Package>, String>,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        async move {
            let package = package.map_err(|error| {
                tracing::error!(%error, "failed to construct the DAP executable package");
                ExecutionError::Internal("failed to construct the DAP executable package")
            })?;
            self.0.execute_async(package, host).await
        }
    }
}

impl ProgramExecutor for DapProgramExecutor {
    fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Self {
        Self(miden_debug::DapExecutor::new(stack_inputs, advice_inputs, options))
    }

    fn execute<H: Host + Send>(
        self,
        program: &Program,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        let package = build_dap_package(program, &PackageDebugInfo::default(), None);
        self.execute_package(package, host)
    }

    fn execute_with_package_debug_info<H: Host + Send>(
        self,
        program: &Program,
        package_debug_info: &PackageDebugInfo,
        entrypoint_source_node: Option<DebugSourceNodeId>,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        let package = build_dap_package(program, package_debug_info, entrypoint_source_node);
        self.execute_package(package, host)
    }
}

/// Wraps the transaction executor's program and its separately-owned debug information in the
/// executable package expected by [`miden_debug::DapExecutor`].
///
/// This conversion lives at the DAP adapter boundary because [`ProgramExecutor`] supplies a
/// [`Program`], while the debugger consumes a [`Package`], and the package API has no constructor
/// for rebuilding that package directly from a program. It should disappear once those upstream
/// executor interfaces agree on a common input type.
fn build_dap_package(
    program: &Program,
    package_debug_info: &PackageDebugInfo,
    entrypoint_source_node: Option<DebugSourceNodeId>,
) -> Result<Arc<Package>, String> {
    // A transaction program is an executable whose root is exported as `$exec::$main`. Reusing
    // the program's MAST forest, entrypoint node, and digest makes `Package::try_into_program`
    // reconstruct the exact program that the transaction executor supplied.
    let entrypoint: Arc<Path> = Path::exec_path().join(ProcedureName::MAIN_PROC_NAME).into();
    let export =
        ProcedureExport::new(entrypoint.clone(), Some(program.entrypoint()), program.hash(), None)
            .with_source_node(entrypoint_source_node);

    // The transaction kernel is an external dependency of transaction programs. The package
    // manifest identifies that dependency, while the embedded kernel section gives the debugger
    // the code required to resolve and execute it offline.
    let kernel = TransactionKernel::package();
    let mut package = Package::create(
        "miden-client-debug".into(),
        kernel.version.clone(),
        TargetType::Executable,
        program.mast_forest().clone(),
        [PackageExport::Procedure(export)],
        [kernel.to_dependency()],
    )
    .map_err(|error| error.to_string())?;

    // `Package::create` installs the export, but executable packages also require the manifest's
    // entrypoint field and the serialized kernel payload.
    package.manifest = package
        .manifest
        .clone()
        .with_entrypoint(entrypoint)
        .map_err(|error| error.to_string())?;
    package.sections.push(Section::new(SectionId::KERNEL, kernel.to_bytes()));

    // Debug information is owned beside `Program` at the transaction-executor boundary. Copy each
    // optional table into its well-known package section so the DAP executor can recover source
    // locations, functions, variables, and the source graph.
    push_debug_section(&mut package, SectionId::DEBUG_TYPES, package_debug_info.types());
    push_debug_section(&mut package, SectionId::DEBUG_SOURCES, package_debug_info.sources());
    push_debug_section(&mut package, SectionId::DEBUG_FUNCTIONS, package_debug_info.functions());
    push_debug_section(
        &mut package,
        SectionId::DEBUG_SOURCE_GRAPH,
        package_debug_info.source_graph(),
    );
    push_debug_section(&mut package, SectionId::DEBUG_SOURCE_MAP, package_debug_info.source_map());
    push_debug_section(
        &mut package,
        SectionId::DEBUG_ERROR_MESSAGES,
        package_debug_info.error_messages(),
    );

    // Decode once here to reject incomplete or internally inconsistent debug tables before the
    // package reaches the asynchronous DAP session.
    package.debug_info().map_err(|error| error.to_string())?;

    Ok(Arc::new(package))
}

fn push_debug_section<T: Serializable>(package: &mut Package, id: SectionId, section: Option<&T>) {
    if let Some(section) = section {
        package.sections.push(Section::new(id, section.to_bytes()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_packages_preserve_transaction_programs() {
        let programs = [
            (
                TransactionKernel::main(),
                TransactionKernel::main_debug_info().unwrap_or_default(),
                TransactionKernel::main_entrypoint_source_node(),
            ),
            (
                TransactionKernel::tx_script_main(),
                TransactionKernel::tx_script_main_debug_info().unwrap_or_default(),
                TransactionKernel::tx_script_main_entrypoint_source_node(),
            ),
        ];

        for (program, debug_info, entrypoint_source_node) in programs {
            let package = build_dap_package(&program, &debug_info, entrypoint_source_node)
                .expect("failed to construct debug package");

            assert_eq!(package.try_into_program().unwrap(), program);
            assert_eq!(package.debug_info().unwrap().unwrap_or_default(), *debug_info);
        }
    }
}

//! Builds the [`Package`] the debug engine needs from a [`Program`] and its debug info.

use alloc::sync::Arc;

use miden_processor::{ExecutionError, Program};
use miden_protocol::assembly::{Path, ProcedureName};
use miden_protocol::transaction::TransactionKernel;
use miden_protocol::utils::serde::Serializable;
use miden_protocol::vm::{
    DebugSourceNodeId,
    Package,
    PackageDebugInfo,
    PackageDebugInfoError,
    PackageExport,
    ProcedureExport,
    Section,
    SectionId,
    TargetType,
};

/// Wraps a program and its debug info into an executable package.
pub(crate) fn build_debug_package(
    program: &Program,
    package_debug_info: &PackageDebugInfo,
    entrypoint_source_node: Option<DebugSourceNodeId>,
) -> Result<Arc<Package>, ExecutionError> {
    // The root is exported as `$exec::$main`, so `Package::try_into_program` gives the same
    // program.
    let entrypoint: Arc<Path> = Path::exec_path().join(ProcedureName::MAIN_PROC_NAME).into();
    let export =
        ProcedureExport::new(entrypoint.clone(), Some(program.entrypoint()), program.hash(), None)
            .with_source_node(entrypoint_source_node);

    // Every transaction program depends on the transaction kernel.
    let kernel = TransactionKernel::package();
    let mut package = Package::create(
        "miden-client-debug".into(),
        kernel.version.clone(),
        TargetType::Executable,
        program.mast_forest().clone(),
        [PackageExport::Procedure(export)],
        [kernel.to_dependency()],
    )
    .map_err(|error| {
        ExecutionError::from(PackageDebugInfoError::InvalidReference {
            message: format!("failed to construct debug executable package: {error}"),
        })
    })?;

    // Kernel code goes along, so the debugger needs no package store.
    package.sections.push(Section::new(SectionId::KERNEL, kernel.to_bytes()));

    // One section holds all debug tables.
    package
        .sections
        .push(Section::new(SectionId::DEBUG_INFO, package_debug_info.to_bytes()));

    // Fail on bad debug references here, not inside the engine.
    package.debug_info()?;

    Ok(Arc::new(package))
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
            let package = build_debug_package(&program, &debug_info, entrypoint_source_node)
                .expect("failed to construct debug package");

            assert_eq!(package.try_into_program().unwrap(), program);
            assert_eq!(package.debug_info().unwrap().unwrap_or_default(), *debug_info);
            assert_eq!(
                package
                    .sections
                    .iter()
                    .filter(|section| section.id == SectionId::DEBUG_INFO)
                    .count(),
                1
            );
        }
    }
}

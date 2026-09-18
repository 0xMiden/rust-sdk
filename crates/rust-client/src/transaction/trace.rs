//! Call tracing in two passes: the real run through the transaction host is recorded as a
//! [`ReplaySnapshot`], then the debug engine replays it cycle by cycle and builds the call tree.
//!
//! The engine owns the trace. This file only holds the recording [`ProgramExecutor`], because the
//! engine cannot depend on `miden-tx`.

use std::sync::Mutex;

pub use miden_debug_engine::debug::{CallFrameRecord, CallTrace, TracedArg};
pub use miden_debug_engine::exec::{CallTraceReplay, replay_call_trace};
use miden_debug_engine::exec::{
    EventMutationRecorder,
    MastForestRecorder,
    RecordingHost,
    ReplaySnapshot,
};
use miden_processor::advice::{AdviceError, AdviceInputs};
use miden_processor::{
    ExecutionError,
    ExecutionOptions,
    ExecutionOutput,
    FastProcessor,
    FutureMaybeSend,
    Host,
    Program,
    StackInputs,
};
use miden_protocol::vm::{DebugSourceNodeId, PackageDebugInfo};
use miden_tx::ProgramExecutor;

use super::package::build_debug_package;

/// Slot for the last recording. `with_program_executor` takes a type, not a value, so the recording
/// cannot leave through a handle. One traced execution at a time.
static RECORDING: Mutex<Option<ReplaySnapshot>> = Mutex::new(None);

/// Takes the last recording out of the slot.
pub(crate) fn take_recording() -> Option<ReplaySnapshot> {
    RECORDING.lock().expect("trace recording slot poisoned").take()
}

/// The normal executor plus a [`ReplaySnapshot`] left in the slot. Holds the inputs until `execute`
/// has the program to build the package with.
pub(crate) struct TraceProgramExecutor {
    /// Built in `new`, so bad inputs fail at the same point as in the default executor.
    processor: FastProcessor,
    stack_inputs: StackInputs,
    advice_inputs: AdviceInputs,
    options: ExecutionOptions,
    package_debug_info: Option<PackageDebugInfo>,
    entrypoint_source_node: Option<DebugSourceNodeId>,
}

impl ProgramExecutor for TraceProgramExecutor {
    fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Result<Self, AdviceError> {
        let processor =
            <FastProcessor as ProgramExecutor>::new(stack_inputs, advice_inputs.clone(), options)?;
        Ok(Self {
            processor,
            stack_inputs,
            advice_inputs,
            options,
            package_debug_info: None,
            entrypoint_source_node: None,
        })
    }

    fn with_debug_info(mut self, package_debug_info: PackageDebugInfo) -> Self {
        self.processor =
            ProgramExecutor::with_debug_info(self.processor, package_debug_info.clone());
        self.package_debug_info = Some(package_debug_info);
        self
    }

    fn with_entrypoint_source_node(
        mut self,
        entrypoint_source_node: Option<DebugSourceNodeId>,
    ) -> Self {
        self.processor =
            ProgramExecutor::with_entrypoint_source_node(self.processor, entrypoint_source_node);
        self.entrypoint_source_node = entrypoint_source_node;
        self
    }

    /// Runs the program like the default executor, with the host wrapped to record its answers.
    fn execute<H: Host + Send>(
        self,
        program: &Program,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        async move {
            let package = build_debug_package(
                program,
                &self.package_debug_info.unwrap_or_default(),
                self.entrypoint_source_node,
            )?;

            let event_recorder = EventMutationRecorder::new();
            let forest_recorder = MastForestRecorder::new();
            let mut recording_host = RecordingHost::new(
                host,
                Some(event_recorder.clone()),
                Some(forest_recorder.clone()),
            );

            // The inherent `FastProcessor::execute` ignores debug info; the trait method does not.
            let result =
                ProgramExecutor::execute(self.processor, program, &mut recording_host).await;

            // Kept on failure too, for the trace up to the failure.
            let recording = ReplaySnapshot {
                package,
                stack_inputs: self.stack_inputs,
                advice_inputs: self.advice_inputs,
                options: self.options,
                event_log: event_recorder.take(),
                mast_forests: forest_recorder.snapshot(),
            };
            *RECORDING.lock().expect("trace recording slot poisoned") = Some(recording);

            result
        }
    }
}

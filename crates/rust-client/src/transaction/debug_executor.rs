//! Internal plumbing for
//! [`Client::execute_program_with_debugger`](crate::Client::execute_program_with_debugger):
//! routes MASM `debug.*` / `trace.*` output to a caller-provided [`fmt::Write`] sink instead of
//! stdout (a no-op on `wasm32-unknown-unknown`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use miden_processor::advice::{AdviceInputs, AdviceMutation};
use miden_processor::event::EventError;
use miden_processor::mast::MastForest;
use miden_processor::operation::DebugOptions;
use miden_processor::{
    BaseHost,
    DebugError,
    DebugHandler,
    DefaultDebugHandler,
    ExecutionError,
    ExecutionOptions,
    ExecutionOutput,
    FastProcessor,
    FutureMaybeSend,
    Host,
    ProcessorState,
    Program,
    StackInputs,
    TraceError,
};
use miden_protocol::Word;
use miden_protocol::assembly::debuginfo::Location;
use miden_protocol::assembly::{SourceFile, SourceSpan};
use miden_protocol::vm::{EventId, EventName};
use miden_tx::ProgramExecutor;

/// Wraps a host, delegating everything except the debug/trace hooks, which go to `handler`. The VM
/// only invokes those hooks in debug/tracing mode, so wrapping a non-debug host is a no-op.
struct DebugRoutingHost<'inner, H, D: DebugHandler> {
    inner: &'inner mut H,
    handler: D,
}

impl<H: BaseHost, D: DebugHandler> BaseHost for DebugRoutingHost<'_, H, D> {
    fn get_label_and_source_file(
        &self,
        location: &Location,
    ) -> (SourceSpan, Option<Arc<SourceFile>>) {
        self.inner.get_label_and_source_file(location)
    }

    fn resolve_event(&self, event_id: EventId) -> Option<&EventName> {
        self.inner.resolve_event(event_id)
    }

    fn on_debug(
        &mut self,
        process: &ProcessorState,
        options: &DebugOptions,
    ) -> Result<(), DebugError> {
        self.handler.on_debug(process, options)
    }

    fn on_trace(&mut self, process: &ProcessorState, trace_id: u32) -> Result<(), TraceError> {
        self.handler.on_trace(process, trace_id)
    }
}

impl<H: Host, D: DebugHandler + Send> Host for DebugRoutingHost<'_, H, D> {
    fn get_mast_forest(&self, node_digest: &Word) -> impl FutureMaybeSend<Option<Arc<MastForest>>> {
        self.inner.get_mast_forest(node_digest)
    }

    fn on_event(
        &mut self,
        process: &ProcessorState,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        self.inner.on_event(process)
    }
}

/// A [`ProgramExecutor`] running on [`FastProcessor`] that routes `debug.*` / `trace.*` output to a
/// [`DefaultDebugHandler`] backed by the writer `W` (default-constructed per execution; hence the
/// `Default` bound, plus `Sync`/`Send` for the handler and returned future).
pub(crate) struct RoutedDebugExecutor<W> {
    processor: FastProcessor,
    _writer: PhantomData<W>,
}

impl<W> ProgramExecutor for RoutedDebugExecutor<W>
where
    W: fmt::Write + Default + Send + Sync + 'static,
{
    fn new(
        stack_inputs: StackInputs,
        advice_inputs: AdviceInputs,
        options: ExecutionOptions,
    ) -> Self {
        let processor = FastProcessor::new_with_options(stack_inputs, advice_inputs, options)
            .expect("constructing FastProcessor failed due to invalid advice inputs");
        Self { processor, _writer: PhantomData }
    }

    fn execute<H: Host + Send>(
        self,
        program: &Program,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        async move {
            let handler = DefaultDebugHandler::new(W::default());
            let mut routing_host = DebugRoutingHost { inner: host, handler };
            FastProcessor::execute(self.processor, program, &mut routing_host).await
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec;
    use core::cell::RefCell;
    use core::fmt;

    use miden_processor::mast::{BasicBlockNodeBuilder, MastForest, MastForestContributor};
    use miden_processor::operation::{Decorator, Operation};
    use miden_processor::{DefaultHost, ExecutionOptions, Program, StackInputs};

    use super::*;

    // Per-thread buffer (each `#[tokio::test]` runs on its own thread, so the tests don't race).
    // The executor default-constructs its writer, so the sink is reached via a thread-local.
    std::thread_local! {
        static CAPTURED: RefCell<String> = const { RefCell::new(String::new()) };
    }

    #[derive(Default)]
    struct CapturingWriter;

    impl fmt::Write for CapturingWriter {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            CAPTURED.with(|c| c.borrow_mut().push_str(s));
            Ok(())
        }
    }

    /// A one-block program (`noop`) with a `debug.stack` decorator attached before the block.
    fn debug_program() -> Program {
        let mut forest = MastForest::new();
        let decorator_id = forest.add_decorator(Decorator::Debug(DebugOptions::StackAll)).unwrap();
        let block_id = BasicBlockNodeBuilder::new(vec![Operation::Noop], vec![])
            .with_before_enter(vec![decorator_id])
            .add_to_forest(&mut forest)
            .unwrap();
        forest.make_root(block_id);
        Program::new(forest.into(), block_id)
    }

    async fn run_captured(debug_mode: bool) -> String {
        CAPTURED.with(|c| c.borrow_mut().clear());
        let program = debug_program();
        let mut host = DefaultHost::default();
        let options = ExecutionOptions::default().with_debugging(debug_mode);
        let executor = RoutedDebugExecutor::<CapturingWriter>::new(
            StackInputs::default(),
            AdviceInputs::default(),
            options,
        );
        executor.execute(&program, &mut host).await.expect("execution should succeed");
        CAPTURED.with(|c| c.borrow().to_string())
    }

    #[tokio::test]
    async fn routes_debug_output_only_in_debug_mode() {
        assert!(
            run_captured(true).await.to_lowercase().contains("stack"),
            "`debug.stack` output should reach the routed writer in debug mode"
        );
        assert!(
            run_captured(false).await.is_empty(),
            "no debug output should be produced when debug mode is off"
        );
    }
}

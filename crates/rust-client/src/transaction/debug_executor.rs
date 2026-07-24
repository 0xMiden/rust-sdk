//! Internal plumbing for
//! [`Client::execute_program_with_debugger`](crate::Client::execute_program_with_debugger):
//! routes MASM debug output to a caller-provided [`fmt::Write`] sink instead of stdout (a no-op on
//! `wasm32-unknown-unknown`).
//!
//! MASM debug output is event-driven: the `miden::core::debug::print_*` procedures emit events that
//! the core library's [`DebugPrinter`] renders to a writer, and the writer the transaction host
//! installs by default is [`StdoutWriter`](miden_processor::StdoutWriter). Since the host builds
//! that registry itself with no override point, this module intercepts the print events one level
//! up, in [`Host::on_event`], and renders them through its own [`DebugPrinter`] before the inner
//! host ever sees them.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt;
use core::marker::PhantomData;

use miden_core_lib::handlers::debug::{
    DebugPrinter,
    PRINT_ADV_MAP_EVENT_NAME,
    PRINT_ADV_MAP_ITEM_EVENT_NAME,
    PRINT_ADV_STACK_EVENT_NAME,
    PRINT_MEM_ALL_EVENT_NAME,
    PRINT_MEM_EVENT_NAME,
    PRINT_STACK_EVENT_NAME,
};
use miden_processor::advice::{AdviceInputs, AdviceMutation};
use miden_processor::event::{EventError, EventHandler, EventHandlerRegistry, EventId, EventName};
use miden_processor::{
    BaseHost,
    ExecutionError,
    ExecutionOptions,
    ExecutionOutput,
    FastProcessor,
    FutureMaybeSend,
    Host,
    LoadedMastForest,
    ProcessorState,
    Program,
    StackInputs,
};
use miden_protocol::Word;
use miden_protocol::assembly::debuginfo::Location;
use miden_protocol::assembly::{SourceFile, SourceSpan};
use miden_protocol::vm::{DebugSourceNodeId, PackageDebugInfo};
use miden_tx::ProgramExecutor;

/// Every `miden::core::debug` print event. The core library registers only the stack and memory
/// printers by default (the advice printers can expose witness data), but routing is per-execution
/// and caller-driven, so all six are covered here.
const PRINT_EVENT_NAMES: [EventName; 6] = [
    PRINT_STACK_EVENT_NAME,
    PRINT_MEM_EVENT_NAME,
    PRINT_MEM_ALL_EVENT_NAME,
    PRINT_ADV_STACK_EVENT_NAME,
    PRINT_ADV_MAP_EVENT_NAME,
    PRINT_ADV_MAP_ITEM_EVENT_NAME,
];

/// Builds a registry holding a single [`DebugPrinter`] backed by `W`, registered for every event in
/// [`PRINT_EVENT_NAMES`].
fn debug_registry<W>(writer: W) -> EventHandlerRegistry
where
    W: fmt::Write + Send + Sync + 'static,
{
    let mut registry = EventHandlerRegistry::new();
    let printer: Arc<dyn EventHandler> = Arc::new(DebugPrinter::new(writer));
    for name in PRINT_EVENT_NAMES {
        registry
            .register(name, printer.clone())
            .expect("`miden::core::debug` print events are distinct and non-reserved");
    }
    registry
}

/// Wraps a host, delegating everything except the `miden::core::debug` print events, which are
/// rendered by `registry` instead of reaching `inner`.
struct DebugRoutingHost<'inner, H> {
    inner: &'inner mut H,
    registry: EventHandlerRegistry,
}

impl<H: BaseHost> BaseHost for DebugRoutingHost<'_, H> {
    fn get_label_and_source_file(
        &self,
        location: &Location,
    ) -> (SourceSpan, Option<Arc<SourceFile>>) {
        self.inner.get_label_and_source_file(location)
    }

    fn resolve_event(&self, event_id: EventId) -> Option<&EventName> {
        self.registry
            .resolve_event(event_id)
            .or_else(|| self.inner.resolve_event(event_id))
    }
}

impl<H: Host + Send> Host for DebugRoutingHost<'_, H> {
    fn get_mast_forest(
        &self,
        node_digest: &Word,
    ) -> impl FutureMaybeSend<Option<LoadedMastForest>> {
        self.inner.get_mast_forest(node_digest)
    }

    fn on_event(
        &mut self,
        process: &ProcessorState<'_>,
    ) -> impl FutureMaybeSend<Result<Vec<AdviceMutation>, EventError>> {
        // The event id sits at the top of the stack (position 0).
        let event_id = EventId::from_felt(process.get_stack_item(0));
        let routed = self.registry.handle_event(event_id, process);
        async move {
            match routed {
                // A print event: already rendered to the routed writer, don't forward it.
                Ok(Some(mutations)) => Ok(mutations),
                Ok(None) => self.inner.on_event(process).await,
                Err(err) => Err(err),
            }
        }
    }
}

/// A [`ProgramExecutor`] running on [`FastProcessor`] that renders `miden::core::debug` print
/// output through a [`DebugPrinter`] backed by the writer `W`. `W` is default-constructed per
/// execution, hence the `Default` bound, plus `Send`/`Sync` for the handler and returned future.
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
            let mut routing_host = DebugRoutingHost {
                inner: host,
                registry: debug_registry(W::default()),
            };
            FastProcessor::execute(self.processor, program, &mut routing_host).await
        }
    }

    fn execute_with_package_debug_info<H: Host + Send>(
        self,
        program: &Program,
        package_debug_info: &PackageDebugInfo,
        entrypoint_source_node: Option<DebugSourceNodeId>,
        host: &mut H,
    ) -> impl FutureMaybeSend<Result<ExecutionOutput, ExecutionError>> {
        async move {
            let mut routing_host = DebugRoutingHost {
                inner: host,
                registry: debug_registry(W::default()),
            };
            match entrypoint_source_node {
                Some(source_node) => {
                    FastProcessor::execute_with_package_debug_info_at_source_node(
                        self.processor,
                        program,
                        package_debug_info,
                        source_node,
                        &mut routing_host,
                    )
                    .await
                },
                None => {
                    FastProcessor::execute_with_package_debug_info(
                        self.processor,
                        program,
                        package_debug_info,
                        &mut routing_host,
                    )
                    .await
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec;
    use core::cell::RefCell;
    use core::fmt;

    use miden_processor::mast::{BasicBlockNodeBuilder, MastForest};
    use miden_processor::operation::Operation;
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

    /// A one-block program that emits `event_name`, i.e. pushes the event id and runs `emit`.
    fn emitting_program(event_name: &EventName) -> Program {
        let mut forest = MastForest::new();
        let event_id = event_name.to_event_id().as_felt();
        let block_id = BasicBlockNodeBuilder::new(vec![
            Operation::Push(event_id),
            Operation::Emit,
            Operation::Drop,
        ])
        .add_to_forest(&mut forest)
        .unwrap();
        forest.make_root(block_id);
        Program::new(forest.into(), block_id)
    }

    async fn run_captured(program: &Program) -> String {
        CAPTURED.with(|c| c.borrow_mut().clear());
        let mut host = DefaultHost::default();
        let executor = RoutedDebugExecutor::<CapturingWriter>::new(
            StackInputs::default(),
            AdviceInputs::default(),
            ExecutionOptions::default(),
        );
        executor.execute(program, &mut host).await.expect("execution should succeed");
        CAPTURED.with(|c| c.borrow().to_string())
    }

    #[tokio::test]
    async fn routes_print_stack_output_to_the_writer() {
        let captured = run_captured(&emitting_program(&PRINT_STACK_EVENT_NAME)).await;
        assert!(
            captured.to_lowercase().contains("stack"),
            "`print_stack` output should reach the routed writer, got: {captured:?}"
        );
    }

    #[tokio::test]
    async fn routes_advice_print_events_not_registered_by_the_core_library() {
        // `print_adv_map` is deliberately absent from the core library's default handler set, so
        // this only produces output because the routing host registers it itself.
        let captured = run_captured(&emitting_program(&PRINT_ADV_MAP_EVENT_NAME)).await;
        assert!(
            !captured.is_empty(),
            "`print_adv_map` should be routed even though the core library omits it by default"
        );
    }

    #[tokio::test]
    async fn leaves_non_debug_events_to_the_inner_host() {
        // An unregistered, non-reserved event: the routing host must delegate rather than swallow
        // it, so `DefaultHost` is the one that decides the outcome (an error, as it has no
        // handler).
        let unknown = EventName::new("miden::client::tests::unrouted");
        let program = emitting_program(&unknown);
        let mut host = DefaultHost::default();
        let executor = RoutedDebugExecutor::<CapturingWriter>::new(
            StackInputs::default(),
            AdviceInputs::default(),
            ExecutionOptions::default(),
        );
        CAPTURED.with(|c| c.borrow_mut().clear());
        let result = executor.execute(&program, &mut host).await;
        // The inner `DefaultHost` has no handler for it, so its error is what surfaces. Asserting
        // on the id proves the event reached `inner` rather than being swallowed here.
        let err = result.expect_err("an event with no handler should error").to_string();
        assert!(
            err.contains(&unknown.to_event_id().as_u64().to_string()),
            "the inner host's error for the delegated event should surface, got: {err:?}"
        );
        assert!(
            CAPTURED.with(|c| c.borrow().is_empty()),
            "a non-debug event must not write to the debug sink"
        );
    }
}

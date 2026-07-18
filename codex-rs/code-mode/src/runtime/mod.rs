mod callbacks;
mod globals;
mod module_loader;
mod timers;
mod value;

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::mpsc as std_mpsc;
use std::thread;

use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::CodeModeToolKind;
use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::enabled_tool_metadata;
use codex_protocol::ToolName;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::TaskFailureHandler;
use crate::v8_init::ensure_v8_initialized;

const EXIT_SENTINEL: &str = "__codex_code_mode_exit__";

#[derive(Debug)]
pub(crate) enum RuntimeCommand {
    ToolResponse { id: String, result: JsonValue },
    ToolError { id: String, error_text: String },
    TimeoutFired { id: u64 },
    ObservePendingFrontier,
    Terminate,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum PendingRuntimeMode {
    #[cfg(test)]
    Continue,
    PauseUntilResumed,
}

#[derive(Debug)]
pub(crate) enum RuntimeControlCommand {
    Continue,
    Resume,
    Terminate,
}

#[derive(Debug)]
pub(crate) enum RuntimeEvent {
    Started,
    Pending,
    ContentItem(FunctionCallOutputContentItem),
    YieldRequested,
    ToolCall {
        id: String,
        name: ToolName,
        kind: CodeModeToolKind,
        input: Option<JsonValue>,
    },
    Notify {
        call_id: String,
        text: String,
    },
    /// A workflow `agent(prompt, opts?)` spawn request (§3 async bridge op; §7
    /// invocation ordinal). Structurally mirrors [`RuntimeEvent::ToolCall`]: the
    /// `agent_callback` mints a resolver stored in `pending_tool_calls` under
    /// `id`, stamps `ordinal` synchronously from `RuntimeState.next_agent_ordinal`
    /// (source-ordered even under `Promise.all`), and emits this event for the
    /// cell actor to route to the spawn helper. Emitted only for workflow runs.
    /// This is the pure type surface both `P1-agent-callback` (emits) and
    /// `P1-cellactor-spawn-dispatch` (consumes) build against; no code
    /// constructs it yet.
    #[allow(
        dead_code,
        reason = "constructed by the later agent_callback / cell_actor dispatch tickets"
    )]
    AgentCall {
        id: String,
        ordinal: u64,
        prompt: String,
        opts: AgentCallOpts,
    },
    /// A workflow `phase(title)` narrator/grouping marker. Emitted only for
    /// workflow runs; the protocol `WorkflowPhaseBegin/End` mapping + journaling
    /// that read `title` are later tickets, so the field is not yet consumed by
    /// non-test code.
    Phase {
        #[allow(dead_code, reason = "consumed by the later protocol/journal tickets")]
        title: String,
    },
    /// A workflow `log(msg)` narrator line. Thin alias over the `Notify` path
    /// (same text plumbing, distinct event). Emitted only for workflow runs; the
    /// protocol `WorkflowLog` mapping + journaling that read `message` are later
    /// tickets, so the field is not yet consumed by non-test code.
    WorkflowLog {
        #[allow(dead_code, reason = "consumed by the later protocol/journal tickets")]
        message: String,
    },
    Result {
        stored_value_writes: HashMap<String, JsonValue>,
        error_text: Option<String>,
    },
    ThreadPanicked,
}

pub(crate) fn spawn_runtime(
    stored_values: HashMap<String, JsonValue>,
    request: ExecuteRequest,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pending_mode: PendingRuntimeMode,
    task_failure_handler: Option<TaskFailureHandler>,
) -> Result<
    (
        std_mpsc::Sender<RuntimeCommand>,
        std_mpsc::Sender<RuntimeControlCommand>,
        v8::IsolateHandle,
    ),
    String,
> {
    ensure_v8_initialized()?;

    let (command_tx, command_rx) = std_mpsc::channel();
    let (control_tx, control_rx) = std_mpsc::channel();
    let runtime_command_tx = command_tx.clone();
    let (isolate_handle_tx, isolate_handle_rx) = std_mpsc::sync_channel(1);
    let enabled_tools = request
        .enabled_tools
        .iter()
        .map(enabled_tool_metadata)
        .collect::<Vec<_>>();
    let config = RuntimeConfig {
        tool_call_id: request.tool_call_id,
        enabled_tools,
        source: request.source,
        stored_values,
        workflow: request.workflow,
        args: request.args,
        run_id: request.run_id,
    };

    spawn_supervised_runtime_thread(event_tx.clone(), task_failure_handler, move || {
        run_runtime(
            config,
            event_tx,
            command_rx,
            control_rx,
            pending_mode,
            isolate_handle_tx,
            runtime_command_tx,
        );
    });

    let isolate_handle = isolate_handle_rx
        .recv()
        .map_err(|_| "failed to initialize code mode runtime".to_string())?;
    Ok((command_tx, control_tx, isolate_handle))
}

fn spawn_supervised_runtime_thread(
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    task_failure_handler: Option<TaskFailureHandler>,
    runtime: impl FnOnce() + Send + 'static,
) {
    thread::spawn(move || {
        if catch_unwind(AssertUnwindSafe(runtime)).is_err() {
            if let Some(task_failure_handler) = task_failure_handler {
                task_failure_handler("code-mode V8 runtime thread panicked".to_string());
            }
            let _ = event_tx.send(RuntimeEvent::ThreadPanicked);
        }
    });
}

#[derive(Clone)]
struct RuntimeConfig {
    tool_call_id: String,
    enabled_tools: Vec<EnabledToolMetadata>,
    source: String,
    stored_values: HashMap<String, JsonValue>,
    /// Explicit workflow invocation mode carried from the `ExecuteRequest`.
    workflow: bool,
    /// Invocation JSON carried from `ExecuteRequest::args`; installed read-only as
    /// the `args` global for workflow runs.
    args: Option<JsonValue>,
    /// Host-minted uuid v7 run identifier from `ExecuteRequest::run_id`; exposed
    /// read-only as `workflow.runId` for workflow runs.
    run_id: Option<String>,
}

pub(super) struct RuntimeState {
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pending_tool_calls: HashMap<String, v8::Global<v8::PromiseResolver>>,
    pending_timeouts: HashMap<u64, timers::ScheduledTimeout>,
    stored_values: HashMap<String, JsonValue>,
    stored_value_writes: HashMap<String, JsonValue>,
    enabled_tools: Vec<EnabledToolMetadata>,
    next_tool_call_id: u64,
    next_timeout_id: u64,
    /// Monotonic source-order counter for `agent()` invocations. Stamped onto
    /// each [`RuntimeEvent::AgentCall`] as its `ordinal` (§7 invocation ordinal),
    /// bumped synchronously in `agent_callback` before the promise returns so the
    /// sequence is deterministic across `parallel`/`Promise.all` concurrency and
    /// stable for prefix-replay cache keying. Initialized to `0`. Not yet read by
    /// non-test code; the `agent_callback` ticket wires the bump.
    #[allow(
        dead_code,
        reason = "stamped by the later agent_callback ticket (P1-agent-callback)"
    )]
    next_agent_ordinal: u64,
    tool_call_id: String,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
    exit_requested: bool,
    /// True when this cell is running a workflow script. Set from the explicit
    /// `ExecuteRequest::workflow` invocation mode (never inferred from source),
    /// so it is `true` only for the workflow handler path. Gates the workflow
    /// narrator globals (`phase`/`log`) so they never leak into plain code-mode
    /// exec sessions.
    workflow: bool,
    /// Invocation JSON injected read-only as the `args` global. Only populated
    /// (and only installed) for workflow runs; see
    /// [`codex_code_mode_protocol::ExecuteRequest::args`].
    args: Option<JsonValue>,
    /// Host-minted uuid v7 run identifier exposed read-only as `workflow.runId`.
    /// Only populated (and only installed) for workflow runs; see
    /// [`codex_code_mode_protocol::ExecuteRequest::run_id`].
    run_id: Option<String>,
}

pub(super) enum CompletionState {
    Pending,
    Completed {
        stored_value_writes: HashMap<String, JsonValue>,
        error_text: Option<String>,
    },
}

fn run_runtime(
    config: RuntimeConfig,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    command_rx: std_mpsc::Receiver<RuntimeCommand>,
    control_rx: std_mpsc::Receiver<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
    isolate_handle_tx: std_mpsc::SyncSender<v8::IsolateHandle>,
    runtime_command_tx: std_mpsc::Sender<RuntimeCommand>,
) {
    let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
    let isolate_handle = isolate.thread_safe_handle();
    if isolate_handle_tx.send(isolate_handle).is_err() {
        return;
    }
    isolate.set_host_import_module_dynamically_callback(module_loader::dynamic_import_callback);

    v8::scope!(let scope, isolate);
    let context = v8::Context::new(scope, Default::default());
    let scope = &mut v8::ContextScope::new(scope, context);

    // Workflow-ness is an explicit invocation mode threaded from the workflow
    // handler through `ExecuteRequest::workflow` (§3), never inferred from the
    // source. A plain code-mode `exec` whose source merely resembles a workflow
    // (e.g. it contains `export const meta = { ... }`) therefore never gains the
    // workflow-only narrator globals; only the workflow handler sets this flag.
    let workflow = config.workflow;

    scope.set_slot(RuntimeState {
        event_tx: event_tx.clone(),
        pending_tool_calls: HashMap::new(),
        pending_timeouts: HashMap::new(),
        stored_values: config.stored_values,
        stored_value_writes: HashMap::new(),
        enabled_tools: config.enabled_tools,
        next_tool_call_id: 1,
        next_timeout_id: 1,
        next_agent_ordinal: 0,
        tool_call_id: config.tool_call_id,
        runtime_command_tx,
        exit_requested: false,
        workflow,
        args: config.args,
        run_id: config.run_id,
    });

    if let Err(error_text) = globals::install_globals(scope) {
        send_result(&event_tx, HashMap::new(), Some(error_text));
        return;
    }

    let _ = event_tx.send(RuntimeEvent::Started);

    let pending_promise = match module_loader::evaluate_main_module(scope, &config.source) {
        Ok(pending_promise) => pending_promise,
        Err(error_text) => {
            capture_scope_send_error(scope, &event_tx, Some(error_text));
            return;
        }
    };

    match module_loader::completion_state(scope, pending_promise.as_ref()) {
        CompletionState::Completed {
            stored_value_writes,
            error_text,
        } => {
            send_result(&event_tx, stored_value_writes, error_text);
            return;
        }
        CompletionState::Pending => {}
    }

    let mut pending_promise = pending_promise;
    while let Some(command) =
        next_runtime_command(&event_tx, &command_rx, &control_rx, pending_mode)
    {
        match command {
            RuntimeCommand::Terminate => break,
            RuntimeCommand::ToolResponse { id, result } => {
                if let Err(error_text) =
                    module_loader::resolve_tool_response(scope, &id, Ok(result))
                {
                    capture_scope_send_error(scope, &event_tx, Some(error_text));
                    return;
                }
            }
            RuntimeCommand::ToolError { id, error_text } => {
                if let Err(runtime_error) =
                    module_loader::resolve_tool_response(scope, &id, Err(error_text))
                {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::TimeoutFired { id } => {
                if let Err(runtime_error) = timers::invoke_timeout_callback(scope, id) {
                    capture_scope_send_error(scope, &event_tx, Some(runtime_error));
                    return;
                }
            }
            RuntimeCommand::ObservePendingFrontier => {}
        }

        scope.perform_microtask_checkpoint();
        match module_loader::completion_state(scope, pending_promise.as_ref()) {
            CompletionState::Completed {
                stored_value_writes,
                error_text,
            } => {
                send_result(&event_tx, stored_value_writes, error_text);
                return;
            }
            CompletionState::Pending => {}
        }

        if let Some(promise) = pending_promise.as_ref() {
            let promise = v8::Local::new(scope, promise);
            if promise.state() != v8::PromiseState::Pending {
                pending_promise = None;
            }
        }
    }
}

fn next_runtime_command(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    command_rx: &std_mpsc::Receiver<RuntimeCommand>,
    control_rx: &std_mpsc::Receiver<RuntimeControlCommand>,
    pending_mode: PendingRuntimeMode,
) -> Option<RuntimeCommand> {
    loop {
        match command_rx.try_recv() {
            Ok(command) => return Some(command),
            Err(std_mpsc::TryRecvError::Disconnected) => return None,
            Err(std_mpsc::TryRecvError::Empty) => {}
        }

        let _ = event_tx.send(RuntimeEvent::Pending);
        match pending_mode {
            #[cfg(test)]
            PendingRuntimeMode::Continue => return command_rx.recv().ok(),
            PendingRuntimeMode::PauseUntilResumed => match control_rx.recv().ok()? {
                RuntimeControlCommand::Continue => return command_rx.recv().ok(),
                RuntimeControlCommand::Resume => continue,
                RuntimeControlCommand::Terminate => return Some(RuntimeCommand::Terminate),
            },
        }
    }
}

fn capture_scope_send_error(
    scope: &mut v8::PinScope<'_, '_>,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    error_text: Option<String>,
) {
    let stored_value_writes = scope
        .get_slot::<RuntimeState>()
        .map(|state| state.stored_value_writes.clone())
        .unwrap_or_default();

    send_result(event_tx, stored_value_writes, error_text);
}

fn send_result(
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    stored_value_writes: HashMap<String, JsonValue>,
    error_text: Option<String>,
) {
    let _ = event_tx.send(RuntimeEvent::Result {
        stored_value_writes,
        error_text,
    });
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::time::Duration;

    use pretty_assertions::assert_eq;
    use tokio::sync::mpsc;

    use super::ExecuteRequest;
    use super::PendingRuntimeMode;
    use super::RuntimeCommand;
    use super::RuntimeControlCommand;
    use super::RuntimeEvent;
    use super::spawn_runtime;
    use super::spawn_supervised_runtime_thread;
    use crate::FunctionCallOutputContentItem;

    fn execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            tool_call_id: "call_1".to_string(),
            enabled_tools: Vec::new(),
            source: source.to_string(),
            yield_time_ms: Some(1),
            max_output_tokens: None,
            workflow: false,
            args: None,
            run_id: None,
        }
    }

    /// A workflow-mode request: identical plumbing to [`execute_request`] but with
    /// the explicit `workflow` invocation flag set, which is the only thing that
    /// authorizes the `phase`/`log` narrator globals.
    fn workflow_execute_request(source: &str) -> ExecuteRequest {
        ExecuteRequest {
            workflow: true,
            ..execute_request(source)
        }
    }

    /// A workflow-mode request carrying invocation `args` JSON and a host-minted
    /// `run_id`, exercising the read-only `args` / `workflow.runId` globals.
    fn workflow_execute_request_with_args(
        source: &str,
        args: serde_json::Value,
        run_id: &str,
    ) -> ExecuteRequest {
        ExecuteRequest {
            args: Some(args),
            run_id: Some(run_id.to_string()),
            ..workflow_execute_request(source)
        }
    }

    /// Collect the ordered `text(...)` outputs from a drained event stream.
    fn text_outputs(events: &[RuntimeEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) => {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect()
    }

    /// Assert the terminal `Result` carried no error.
    fn assert_result_ok(events: &[RuntimeEvent]) {
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        assert!(
            error_text.is_none(),
            "workflow body must run cleanly, got: {error_text:?}"
        );
    }

    #[tokio::test]
    async fn runtime_thread_panic_before_initialization_is_reported_directly() {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        drop(event_rx);
        let (failure_tx, mut failure_rx) = mpsc::unbounded_channel();
        spawn_supervised_runtime_thread(
            event_tx,
            Some(std::sync::Arc::new(move |reason| {
                let _ = failure_tx.send(reason);
            })),
            || panic!("runtime thread panic probe"),
        );

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), failure_rx.recv())
                .await
                .expect("runtime failure timeout")
                .expect("runtime failure"),
            "code-mode V8 runtime thread panicked"
        );
    }

    #[tokio::test]
    async fn runtime_thread_panic_is_forwarded_without_owner_supervision() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        spawn_supervised_runtime_thread(
            event_tx,
            /*task_failure_handler*/ None,
            || panic!("runtime thread panic probe"),
        );

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .expect("runtime panic event timeout"),
            Some(RuntimeEvent::ThreadPanicked)
        ));
    }

    #[tokio::test]
    async fn terminate_execution_stops_cpu_bound_module() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, _runtime_control_tx, runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request("while (true) {}"),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let started_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(started_event, RuntimeEvent::Started));

        assert!(runtime_terminate_handle.terminate_execution());

        let result_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeEvent::Result { error_text, .. } = result_event else {
            panic!("expected runtime result after termination");
        };
        assert!(error_text.is_some());

        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn pending_mode_freezes_runtime_commands_until_resume() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (runtime_tx, runtime_control_tx, _runtime_terminate_handle) = spawn_runtime(
            HashMap::new(),
            execute_request(
                r#"
await new Promise((resolve) => setTimeout(resolve, 60_000));
text("after");
await new Promise(() => {});
"#,
            ),
            event_tx,
            PendingRuntimeMode::PauseUntilResumed,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Started
        ));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Pending
        ));

        runtime_tx
            .send(RuntimeCommand::TimeoutFired { id: 1 })
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .is_err()
        );

        runtime_control_tx
            .send(RuntimeControlCommand::Resume)
            .unwrap();

        let content_event = tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let RuntimeEvent::ContentItem(FunctionCallOutputContentItem::InputText { text }) =
            content_event
        else {
            panic!("expected resumed runtime output");
        };
        assert_eq!(text, "after");
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), event_rx.recv())
                .await
                .unwrap()
                .unwrap(),
            RuntimeEvent::Pending
        ));

        runtime_control_tx
            .send(RuntimeControlCommand::Terminate)
            .unwrap();
    }

    /// Drain events until the runtime reports its terminal `Result`, returning
    /// the ordered events observed (including the final `Result`).
    async fn drain_to_result(
        event_rx: &mut mpsc::UnboundedReceiver<RuntimeEvent>,
    ) -> Vec<RuntimeEvent> {
        let mut events = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(5), event_rx.recv())
                .await
                .expect("runtime event timeout")
                .expect("runtime event channel closed");
            let is_result = matches!(event, RuntimeEvent::Result { .. });
            events.push(event);
            if is_result {
                return events;
            }
        }
    }

    #[tokio::test]
    async fn workflow_phase_and_log_emit_events_in_call_order() {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo', phases: ['plan'] };\n",
            "phase('plan');\n",
            "log('hello');\n",
            "phase('build');\n",
        );
        let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        let narrator: Vec<&RuntimeEvent> = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    RuntimeEvent::Phase { .. } | RuntimeEvent::WorkflowLog { .. }
                )
            })
            .collect();

        assert_eq!(narrator.len(), 3, "expected phase/log events: {events:?}");
        assert!(matches!(narrator[0], RuntimeEvent::Phase { title } if title == "plan"));
        assert!(matches!(narrator[1], RuntimeEvent::WorkflowLog { message } if message == "hello"));
        assert!(matches!(narrator[2], RuntimeEvent::Phase { title } if title == "build"));

        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        assert!(error_text.is_none(), "workflow body must run cleanly");
    }

    #[tokio::test]
    async fn plain_exec_does_not_install_workflow_globals() {
        // A plain code-mode program must NOT see `phase`/`log` — calling them
        // throws a `ReferenceError`, surfaced as a runtime error. Crucially the
        // source here is *meta-shaped* (it opens with a valid `export const meta`
        // manifest), yet because the request is NOT in workflow mode the narrator
        // globals stay uninstalled: workflow-ness is the explicit invocation flag,
        // never the source shape.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo', phases: ['plan'] };\n",
            "phase('x');\n",
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::Phase { .. })),
            "plain exec must not emit workflow phase events: {events:?}"
        );
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        let error_text = error_text.as_deref().unwrap_or_default();
        assert!(
            error_text.contains("phase is not defined"),
            "expected ReferenceError for missing `phase` global, got: {error_text}"
        );
    }

    #[tokio::test]
    async fn workflow_log_reuses_notify_text_validation() {
        // `log()` shares `notify`'s text plumbing: empty input is rejected with a
        // `log`-specific message rather than emitting an event.
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "log('   ');\n",
        );
        let (_runtime_tx, _runtime_control_tx, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, RuntimeEvent::WorkflowLog { .. })),
            "empty log must not emit an event: {events:?}"
        );
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        let error_text = error_text.as_deref().unwrap_or_default();
        assert!(
            error_text.contains("log expects non-empty text"),
            "expected the shared narrator validation error, got: {error_text}"
        );
    }

    #[tokio::test]
    async fn workflow_args_global_exposes_invocation_json() {
        // The invocation JSON is injected read-only as the `args` global; a
        // workflow body reads `args.foo` and receives the value passed at
        // invocation (acceptance: "reads args.foo and receives the value").
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "text(String(args.foo));\n",
            "text(JSON.stringify(args.nested));\n",
        );
        let (_tx, _ctrl, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request_with_args(
                source,
                serde_json::json!({ "foo": "bar", "nested": { "n": 1 } }),
                "run-args-1",
            ),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        assert_result_ok(&events);
        assert_eq!(
            text_outputs(&events),
            vec!["bar".to_string(), "{\"n\":1}".to_string()],
        );
    }

    #[tokio::test]
    async fn workflow_run_id_global_is_host_minted_and_stable() {
        // `workflow.runId` returns exactly the host-minted value and is stable
        // across reads within the run (acceptance: "returns the host-minted uuid
        // v7 and is stable within a run").
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "const a = workflow.runId;\n",
            "const b = workflow.runId;\n",
            "text(a);\n",
            "text(String(a === b));\n",
        );
        let (_tx, _ctrl, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request_with_args(
                source,
                serde_json::Value::Null,
                "0192f000-0000-7000-8000-0000000000ab",
            ),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        assert_result_ok(&events);
        assert_eq!(
            text_outputs(&events),
            vec![
                "0192f000-0000-7000-8000-0000000000ab".to_string(),
                "true".to_string(),
            ],
        );
    }

    #[tokio::test]
    async fn workflow_args_and_run_id_are_read_only() {
        // Assignment to `args` or `workflow.runId` must throw or be silently
        // ignored. Wrapping each write in try/catch and reading the value back
        // proves the binding is unchanged under either behavior (acceptance:
        // "assignment throws or is silently ignored, tested").
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "try { args = { foo: 'hacked' }; } catch (_e) {}\n",
            "try { workflow.runId = 'hacked'; } catch (_e) {}\n",
            "text(String(args.foo));\n",
            "text(workflow.runId);\n",
        );
        let (_tx, _ctrl, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request_with_args(
                source,
                serde_json::json!({ "foo": "original" }),
                "run-readonly-1",
            ),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        assert_result_ok(&events);
        assert_eq!(
            text_outputs(&events),
            vec!["original".to_string(), "run-readonly-1".to_string()],
            "read-only args/workflow.runId must be unchanged by assignment",
        );
    }

    #[tokio::test]
    async fn plain_exec_does_not_install_args_or_workflow_globals() {
        // The `args` and `workflow` globals are workflow-only: a plain code-mode
        // exec that reads `args` sees a `ReferenceError`, never a leaked global.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "text(String(args.foo));\n",
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_tx, _ctrl, _handle) = spawn_runtime(
            HashMap::new(),
            execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        let error_text = error_text.as_deref().unwrap_or_default();
        assert!(
            error_text.contains("args is not defined"),
            "expected ReferenceError for missing `args` global, got: {error_text}"
        );
    }

    /// Drive a workflow-mode source and return the ordered `text(...)` outputs,
    /// asserting the terminal `Result` carried no error. Shared by the
    /// `parallel()` prelude tests below.
    async fn run_workflow_text_outputs(source: &str) -> Vec<String> {
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_tx, _ctrl, _handle) = spawn_runtime(
            HashMap::new(),
            workflow_execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();
        let events = drain_to_result(&mut event_rx).await;
        assert_result_ok(&events);
        text_outputs(&events)
    }

    #[tokio::test]
    async fn parallel_returns_results_in_input_order() {
        // Acceptance: `parallel` of N thunks returns an N-length array in input
        // order (position-preserving), independent of completion order.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "const results = await parallel([\n",
            "  async () => 'a',\n",
            "  async () => 'b',\n",
            "  async () => 'c',\n",
            "]);\n",
            "text(String(results.length));\n",
            "text(JSON.stringify(results));\n",
        );
        assert_eq!(
            run_workflow_text_outputs(source).await,
            vec!["3".to_string(), "[\"a\",\"b\",\"c\"]".to_string()],
        );
    }

    #[tokio::test]
    async fn parallel_throwing_thunk_yields_null_without_failing_siblings() {
        // Acceptance: a thunk that throws yields `null` at its position while its
        // siblings still resolve to their values.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "const results = await parallel([\n",
            "  async () => 'ok',\n",
            "  async () => { throw new Error('boom'); },\n",
            "  async () => 'fine',\n",
            "]);\n",
            "text(JSON.stringify(results));\n",
        );
        assert_eq!(
            run_workflow_text_outputs(source).await,
            vec!["[\"ok\",null,\"fine\"]".to_string()],
        );
    }

    #[tokio::test]
    async fn parallel_awaits_all_thunks_before_resolving() {
        // Acceptance: `parallel` is a barrier — it awaits ALL thunks before
        // resolving. The thunks complete in a different order than dispatched
        // (descending `setTimeout` delays), yet at resolution every thunk has run
        // (`completed === 3`) and the results stay position-preserving.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "let completed = 0;\n",
            "const mk = (value, delay) => () =>\n",
            "  new Promise((resolve) =>\n",
            "    setTimeout(() => { completed += 1; resolve(value); }, delay)\n",
            "  );\n",
            "const results = await parallel([\n",
            "  mk('a', 30),\n",
            "  mk('b', 5),\n",
            "  mk('c', 15),\n",
            "]);\n",
            "text(String(completed));\n",
            "text(JSON.stringify(results));\n",
        );
        assert_eq!(
            run_workflow_text_outputs(source).await,
            vec!["3".to_string(), "[\"a\",\"b\",\"c\"]".to_string()],
            "barrier must await all thunks; results stay position-preserving",
        );
    }

    #[tokio::test]
    async fn parallel_at_cap_boundary_dispatches_all() {
        // The 4096-item boundary is inclusive: exactly 4096 thunks dispatch and
        // resolve without tripping the cap guard.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "const thunks = [];\n",
            "for (let i = 0; i < 4096; i++) thunks.push(async () => i);\n",
            "const results = await parallel(thunks);\n",
            "text(String(results.length));\n",
            "text(String(results[0]) + ',' + String(results[4095]));\n",
        );
        assert_eq!(
            run_workflow_text_outputs(source).await,
            vec!["4096".to_string(), "0,4095".to_string()],
        );
    }

    #[tokio::test]
    async fn parallel_over_cap_throws_before_any_dispatch() {
        // Acceptance: >4096 items throws a descriptive cap error BEFORE any thunk
        // is dispatched. `dispatched` staying at 0 proves the guard fires ahead of
        // `Array.prototype.map` calling the thunks.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "let dispatched = 0;\n",
            "const thunks = [];\n",
            "for (let i = 0; i < 4097; i++)\n",
            "  thunks.push(() => { dispatched += 1; return Promise.resolve(i); });\n",
            "try {\n",
            "  await parallel(thunks);\n",
            "  text('NO_THROW');\n",
            "} catch (e) {\n",
            "  text(e.constructor.name);\n",
            "  text(String(e.message.includes('4096')));\n",
            "}\n",
            "text('dispatched=' + dispatched);\n",
        );
        assert_eq!(
            run_workflow_text_outputs(source).await,
            vec![
                "RangeError".to_string(),
                "true".to_string(),
                "dispatched=0".to_string(),
            ],
            "cap must throw a descriptive RangeError before dispatching any thunk",
        );
    }

    #[tokio::test]
    async fn plain_exec_does_not_install_parallel_prelude() {
        // `parallel` is gated on the workflow flag exactly like the other
        // narrator globals: a plain code-mode exec that calls it sees a
        // `ReferenceError`, never a leaked binding.
        let source = concat!(
            "export const meta = { name: 'demo', description: 'demo' };\n",
            "await parallel([async () => 1]);\n",
        );
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let (_tx, _ctrl, _handle) = spawn_runtime(
            HashMap::new(),
            execute_request(source),
            event_tx,
            PendingRuntimeMode::Continue,
            /*task_failure_handler*/ None,
        )
        .unwrap();

        let events = drain_to_result(&mut event_rx).await;
        let RuntimeEvent::Result { error_text, .. } = events.last().expect("result event") else {
            panic!("last event must be Result");
        };
        let error_text = error_text.as_deref().unwrap_or_default();
        assert!(
            error_text.contains("parallel is not defined"),
            "expected ReferenceError for missing `parallel` global, got: {error_text}"
        );
    }
}

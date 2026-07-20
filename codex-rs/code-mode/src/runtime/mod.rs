mod callbacks;
mod events;
mod globals;
mod module_loader;
mod state;
mod timers;
mod value;
mod workflow_context;
mod workflow_progress;

#[cfg(test)]
#[path = "workflow_bounds_tests.rs"]
mod workflow_bounds_tests;

use std::collections::HashMap;
use std::collections::HashSet;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::thread;

use codex_code_mode_protocol::EnabledToolMetadata;
use codex_code_mode_protocol::ExecuteRequest;
use codex_code_mode_protocol::enabled_tool_metadata;
use codex_workflow_journal::AgentCallLine;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;

use crate::TaskFailureHandler;
use crate::v8_init::ensure_v8_initialized;

const EXIT_SENTINEL: &str = "__codex_code_mode_exit__";

pub(crate) use events::PendingRuntimeMode;
pub(crate) use events::RuntimeCommand;
pub(crate) use events::RuntimeControlCommand;
pub(crate) use events::RuntimeEvent;

/// Live, thread-safe view of the runtime-owned workflow budget mirror backing
/// `budget.spent()` / `budget.remaining()`. The serializable execute snapshot
/// seeds this handle and host callbacks refresh it before their promises settle.
pub(crate) use codex_code_mode_protocol::WorkflowBudgetHandle;

/// Budget-less [`spawn_runtime_with_budget`] shim retained for tests that exercise
/// the runtime without a workflow budget handle. Production spawns go through
/// [`spawn_runtime_with_budget`] so they can thread the host's live budget handle
/// (SEAM #1).
#[cfg(test)]
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
    spawn_runtime_with_budget(
        stored_values,
        request,
        event_tx,
        pending_mode,
        task_failure_handler,
        None,
        None,
    )
}

/// [`spawn_runtime`] variant that threads the runtime-owned
/// [`WorkflowBudgetHandle`] into the isolate. Plain code-mode `exec` passes
/// `None`; workflow cells normally seed a mirror from the execute request.
pub(crate) fn spawn_runtime_with_budget(
    stored_values: HashMap<String, JsonValue>,
    request: ExecuteRequest,
    event_tx: mpsc::UnboundedSender<RuntimeEvent>,
    pending_mode: PendingRuntimeMode,
    task_failure_handler: Option<TaskFailureHandler>,
    budget: Option<Arc<dyn WorkflowBudgetHandle>>,
    replay_entries: Option<Vec<AgentCallLine>>,
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
        budget,
        replay_entries,
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
    /// Runtime-owned token-budget mirror backing the workflow `budget` global.
    /// `None` for plain code-mode exec.
    budget: Option<Arc<dyn WorkflowBudgetHandle>>,
    /// Prior-run journal `agent_call` entries seeding prefix-replay on a resumed
    /// run (§7 resume algorithm step 2). `None` for a fresh run — the common
    /// case — which leaves [`ReplayState::fresh`] installed so fan-out behaves
    /// exactly as before. `Some(entries)` arms replay with those entries (an
    /// empty vec is a valid resume of a run that made no `agent()` calls). The
    /// loader/validator that produces the entries is `P3-resume-entry`; this
    /// field is the seam it drives.
    replay_entries: Option<Vec<AgentCallLine>>,
}

use state::ReplayState;
use state::RuntimeState;

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
    let workflow = config.workflow;
    let isolate = &mut v8::Isolate::new(v8::CreateParams::default());
    if workflow {
        isolate.set_promise_hook(workflow_context::promise_hook);
    }
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
        next_workflow_node_id: 0,
        active_workflow_parent_node_id: None,
        active_workflow_phase: None,
        next_workflow_phase_index: 0,
        workflow_log_count: 0,
        workflow_phase_count: 0,
        workflow_output_bounds: codex_code_mode_protocol::WorkflowOutputBounds::default(),
        active_workflow_nodes: HashSet::new(),
        active_workflow_groups: HashSet::new(),
        pending_workflow_agent_nodes: HashMap::new(),
        workflow_parent_context_stack: Vec::new(),
        next_workflow_call_id: 0,
        tool_call_id: config.tool_call_id,
        runtime_command_tx,
        exit_requested: false,
        workflow,
        args: config.args,
        run_id: config.run_id,
        budget: config.budget,
        // Prefix-replay scaffolding (§7 resume step 2): a resumed run seeds the
        // prior journal's `agent_call` entries and arms replay; a fresh run (the
        // common case, `None`) installs the empty/inactive `fresh` state so
        // fan-out behaves exactly as before. The loader that produces the entries
        // is `P3-resume-entry`; the replay DECISION driven off this state is
        // `agent_callback` below.
        replay: match config.replay_entries {
            Some(entries) => ReplayState::seed(entries),
            None => ReplayState::fresh(),
        },
    });

    if let Err(error_text) = globals::install_globals(scope) {
        send_scope_result(scope, &event_tx, HashMap::new(), Some(error_text));
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

    let mut deferred_completion = None;
    match module_loader::completion_state(scope, pending_promise.as_ref()) {
        CompletionState::Completed {
            stored_value_writes,
            error_text,
        } => {
            // A module is allowed to start a `parallel()`/`pipeline()` group without awaiting its
            // returned promise. Keep the isolate alive until those orchestration barriers settle so
            // their `.finally` callbacks can emit GroupEnd before the terminal result. A bare
            // unawaited root `agent()` does not form a barrier; normal cell teardown cancels it. A
            // microtask checkpoint first drains empty/synchronous groups without a host command.
            scope.perform_microtask_checkpoint();
            if workflow_progress::has_active_workflow_groups(scope) {
                deferred_completion = Some((stored_value_writes, error_text));
            } else {
                send_scope_result(scope, &event_tx, stored_value_writes, error_text);
                return;
            }
        }
        CompletionState::Pending => {}
    }

    let mut pending_promise = pending_promise;
    while let Some(command) =
        next_runtime_command(&event_tx, &command_rx, &control_rx, pending_mode)
    {
        match command {
            RuntimeCommand::Terminate => {
                if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
                    state.finish_active_workflow_phase();
                }
                break;
            }
            RuntimeCommand::ToolResponse { id, result } => {
                finish_workflow_agent_node(scope, &id);
                if let Err(error_text) =
                    module_loader::resolve_tool_response(scope, &id, Ok(result))
                {
                    capture_scope_send_error(scope, &event_tx, Some(error_text));
                    return;
                }
            }
            RuntimeCommand::ToolError { id, error_text } => {
                finish_workflow_agent_node(scope, &id);
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
        if let Some((stored_value_writes, error_text)) = deferred_completion.take() {
            if workflow_progress::has_active_workflow_groups(scope) {
                deferred_completion = Some((stored_value_writes, error_text));
            } else {
                send_scope_result(scope, &event_tx, stored_value_writes, error_text);
                return;
            }
        } else {
            match module_loader::completion_state(scope, pending_promise.as_ref()) {
                CompletionState::Completed {
                    stored_value_writes,
                    error_text,
                } => {
                    if workflow_progress::has_active_workflow_groups(scope) {
                        deferred_completion = Some((stored_value_writes, error_text));
                    } else {
                        send_scope_result(scope, &event_tx, stored_value_writes, error_text);
                        return;
                    }
                }
                CompletionState::Pending => {}
            }
        }

        if let Some(promise) = pending_promise.as_ref() {
            let promise = v8::Local::new(scope, promise);
            if promise.state() != v8::PromiseState::Pending {
                pending_promise = None;
            }
        }
    }
}

fn finish_workflow_agent_node(scope: &mut v8::PinScope<'_, '_>, id: &str) {
    let Some(state) = scope.get_slot_mut::<RuntimeState>() else {
        return;
    };
    if let Some(node_id) = state.pending_workflow_agent_nodes.remove(id) {
        state.active_workflow_nodes.remove(&node_id);
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

    send_scope_result(scope, event_tx, stored_value_writes, error_text);
}

fn send_scope_result(
    scope: &mut v8::PinScope<'_, '_>,
    event_tx: &mpsc::UnboundedSender<RuntimeEvent>,
    stored_value_writes: HashMap<String, JsonValue>,
    error_text: Option<String>,
) {
    if let Some(state) = scope.get_slot_mut::<RuntimeState>() {
        state.finish_active_workflow_phase();
    }
    let _ = event_tx.send(RuntimeEvent::Result {
        stored_value_writes,
        error_text,
    });
}

#[cfg(test)]
#[path = "runtime_tests.rs"]
mod tests;

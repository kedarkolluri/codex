mod callbacks;
mod conversions;
mod types;

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use codex_code_mode_protocol::WorkflowHostCompletion;
use codex_code_mode_protocol::WorkflowHostProgress;
use serde_json::Value as JsonValue;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use self::callbacks::CallbackCompletion;
use self::callbacks::finish_callbacks;
use self::callbacks::refresh_workflow_budget;
use self::callbacks::report_task_result;
use self::callbacks::spawn_agent;
use self::callbacks::spawn_notification;
use self::callbacks::spawn_tool;
use self::callbacks::spawn_workflow;
use self::conversions::cell_tool_kind;
use self::conversions::output_item;
use self::conversions::runtime_request;
use self::types::CellCommand;
pub(crate) use self::types::CellError;
pub(crate) use self::types::CellEventFuture;
pub(crate) use self::types::CellHandle;
pub(crate) use self::types::CellHost;
pub(crate) use self::types::CellState;
pub(crate) use self::types::CellToolCall;
pub(crate) use self::types::CompletionCommit;
use self::types::CompletionDelivery;
use self::types::ObservationDelivery;
use crate::TaskFailureHandler;
use crate::runtime::PendingRuntimeMode;
use crate::runtime::RuntimeCommand;
use crate::runtime::RuntimeControlCommand;
use crate::runtime::RuntimeEvent;
use crate::runtime::spawn_runtime_with_budget;
use crate::session_runtime::CellEvent;
use crate::session_runtime::CreateCellRequest as CellRequest;
use crate::session_runtime::ObserveMode;
use crate::session_runtime::OutputItem;
use crate::session_runtime::ToolName as CellToolName;
use crate::workflow_budget::WorkflowBudgetMirror;

const WORKFLOW_JOURNAL_UNAVAILABLE_ERROR: &str = "workflow journal is unavailable";

pub(crate) struct CellActor;

impl CellActor {
    pub(crate) fn prepare<H: CellHost>(
        request: CellRequest,
        stored_values: HashMap<String, JsonValue>,
        host: Arc<H>,
        initial_observe_mode: ObserveMode,
        cell_state: Arc<CellState>,
        task_failure_handler: Option<TaskFailureHandler>,
    ) -> Result<
        (
            CellHandle,
            CellEventFuture,
            impl Future<Output = ()> + Send + 'static,
        ),
        String,
    > {
        let workflow = request.workflow;
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (initial_response_tx, initial_response_rx) = oneshot::channel();
        // Both in-process and process-owned cells use the same runtime-local
        // mirror. Its initial value is serializable on the execute request and
        // callback acknowledgements refresh it before the JS promise settles.
        let budget = request
            .workflow_budget
            .map(WorkflowBudgetMirror::new)
            .map(Arc::new);
        let runtime_budget = budget.as_ref().map(|budget| {
            Arc::clone(budget) as Arc<dyn codex_code_mode_protocol::WorkflowBudgetHandle>
        });
        // Prefix-replay seed (SEAM, `P3-resume-entry`, spec §7 steps 1-3). The resume
        // entrypoint stashes the prior run's loaded journal `agent_call` lines on the
        // host; the host hands them to the FIRST cell it spawns (the top-level resumed
        // run) and returns empty for every later nested `workflow()` cell. An empty vec
        // maps to `None` so a fresh run installs `ReplayState::fresh` and dispatches
        // every `agent()` live, exactly as before.
        let replay_entries = request.replay_entries.clone();
        let replay_entries = if replay_entries.is_empty() {
            None
        } else {
            Some(replay_entries)
        };
        let (runtime_tx, runtime_control_tx, runtime_terminate_handle) = spawn_runtime_with_budget(
            stored_values,
            runtime_request(request),
            event_tx,
            PendingRuntimeMode::PauseUntilResumed,
            task_failure_handler.clone(),
            runtime_budget,
            replay_entries,
        )?;
        let handle = CellHandle::new(command_tx, Arc::clone(&cell_state));
        let task = run_cell(
            host,
            CellContext {
                runtime_tx,
                runtime_control_tx,
                runtime_terminate_handle,
                cell_state,
                workflow,
                budget,
            },
            event_rx,
            command_rx,
            Observer {
                mode: initial_observe_mode,
                response_tx: initial_response_tx,
            },
            task_failure_handler,
        );
        let initial_response =
            Box::pin(async move { initial_response_rx.await.unwrap_or(Err(CellError::Closed)) });
        Ok((handle, initial_response, task))
    }
}

struct CellContext {
    runtime_tx: std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_control_tx: std::sync::mpsc::Sender<RuntimeControlCommand>,
    runtime_terminate_handle: v8::IsolateHandle,
    cell_state: Arc<CellState>,
    workflow: bool,
    budget: Option<Arc<WorkflowBudgetMirror>>,
}

struct Observer {
    mode: ObserveMode,
    response_tx: oneshot::Sender<Result<CellEvent, CellError>>,
}

async fn run_cell<H: CellHost>(
    host: Arc<H>,
    context: CellContext,
    mut event_rx: mpsc::UnboundedReceiver<RuntimeEvent>,
    command_rx: mpsc::UnboundedReceiver<CellCommand>,
    initial_observer: Observer,
    task_failure_handler: Option<TaskFailureHandler>,
) {
    let CellContext {
        runtime_tx,
        runtime_control_tx,
        runtime_terminate_handle,
        cell_state,
        workflow,
        budget,
    } = context;
    let cancellation_token = cell_state.cancellation_token();
    let callback_cancellation_token = cancellation_token.child_token();
    let mut content_items = Vec::new();
    let mut pending_tool_call_ids = Vec::new();
    let mut pending_frontier_ready = false;
    let mut observer = Some(initial_observer);
    let mut termination = false;
    let mut runtime_closed = false;
    let mut runtime_paused = false;
    let mut runtime_failure_reported = false;
    let mut journal_failed = false;
    let mut yield_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>> = None;
    let mut notification_tasks = JoinSet::new();
    let mut tool_tasks = JoinSet::new();
    let mut command_rx = Some(command_rx);
    loop {
        let yield_deadline_elapsed = yield_timer
            .as_ref()
            .is_some_and(|yield_timer| yield_timer.deadline() <= tokio::time::Instant::now());
        tokio::select! {
            biased;
            _ = cancellation_token.cancelled(), if !termination => {
                termination = true;
                yield_timer = None;
                drop(command_rx.take());
                begin_termination(
                    &runtime_tx,
                    &runtime_control_tx,
                    &runtime_terminate_handle,
                    &cancellation_token,
                );
                if runtime_closed {
                    finish_callbacks(
                        &callback_cancellation_token,
                        &mut notification_tasks,
                        &mut tool_tasks,
                        CallbackCompletion::Cancel,
                        task_failure_handler.as_ref(),
                    ).await;
                    report_workflow_completion(
                        host.as_ref(),
                        workflow,
                        WorkflowHostCompletion::Interrupted,
                    )
                    .await;
                    finish_termination(
                        &cell_state,
                        observer.take().map(|observer| observer.response_tx),
                        CellEvent::Terminated {
                            content_items: std::mem::take(&mut content_items),
                        },
                    );
                    break;
                }
            }
            maybe_command = async {
                match command_rx.as_mut() {
                    Some(command_rx) => command_rx.recv().await,
                    None => std::future::pending::<Option<CellCommand>>().await,
                }
            } => {
                let Some(CellCommand::Observe { mode, response_tx }) = maybe_command else {
                    cancellation_token.cancel();
                    continue;
                };
                if response_tx.is_closed() {
                    continue;
                }
                let response_tx = match cell_state.route_observation(mode, response_tx) {
                    ObservationDelivery::Running(response_tx) => response_tx,
                    ObservationDelivery::Delivered => break,
                    ObservationDelivery::Buffered | ObservationDelivery::Closed => continue,
                };
                if observer
                    .as_ref()
                    .is_some_and(|observer| observer.response_tx.is_closed())
                {
                    observer = None;
                    yield_timer = None;
                }
                if observer.is_some() || termination {
                    let _ = response_tx.send(Err(CellError::Busy));
                    continue;
                }
                if matches!(mode, ObserveMode::PendingFrontier) && pending_frontier_ready {
                    pending_frontier_ready = false;
                    match send_cell_event(
                        response_tx,
                        CellEvent::Pending {
                            content_items: std::mem::take(&mut content_items),
                            pending_tool_call_ids: std::mem::take(&mut pending_tool_call_ids),
                        },
                    ) {
                        Ok(()) => {}
                        Err(CellEvent::Pending {
                            content_items: undelivered_items,
                            pending_tool_call_ids: undelivered_tool_call_ids,
                        }) => {
                            content_items = undelivered_items;
                            pending_tool_call_ids = undelivered_tool_call_ids;
                            pending_frontier_ready = true;
                        }
                        Err(event) => {
                            panic!("pending delivery returned an unexpected event: {event:?}")
                        }
                    }
                    continue;
                }
                observer = Some(Observer { mode, response_tx });
                yield_timer = observer.as_ref().and_then(observer_timer);
                if runtime_paused && matches!(mode, ObserveMode::YieldAfter(_)) {
                    pending_frontier_ready = false;
                    pending_tool_call_ids.clear();
                }
                resume_for_observation(
                    mode,
                    &mut runtime_paused,
                    &runtime_tx,
                    &runtime_control_tx,
                );
            }
            _ = async {
                if let Some(yield_timer) = yield_timer.as_mut() {
                    yield_timer.await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                yield_timer = None;
                restore_undelivered_yield(
                    send_observer_event(
                        observer.take(),
                        CellEvent::Yielded {
                            content_items: std::mem::take(&mut content_items),
                        },
                    ),
                    &mut content_items,
                );
            }
            maybe_event = async {
                if runtime_closed {
                    std::future::pending::<Option<RuntimeEvent>>().await
                } else {
                    event_rx.recv().await
                }
            }, if !yield_deadline_elapsed => {
                let Some(event) = maybe_event else {
                    runtime_closed = true;
                    if termination || cancellation_token.is_cancelled() {
                        finish_callbacks(
                            &callback_cancellation_token,
                            &mut notification_tasks,
                            &mut tool_tasks,
                            CallbackCompletion::Cancel,
                            task_failure_handler.as_ref(),
                        ).await;
                        report_workflow_completion(
                            host.as_ref(),
                            workflow,
                            WorkflowHostCompletion::Interrupted,
                        )
                        .await;
                        finish_termination(
                            &cell_state,
                            observer.take().map(|observer| observer.response_tx),
                            CellEvent::Terminated {
                                content_items: std::mem::take(&mut content_items),
                            },
                        );
                        break;
                    }
                    if !journal_failed
                        && !runtime_failure_reported
                        && let Some(task_failure_handler) = &task_failure_handler
                    {
                        runtime_failure_reported = true;
                        task_failure_handler(
                            "code-mode V8 runtime thread ended unexpectedly".to_string(),
                        );
                    }
                    let callback_completion = if journal_failed {
                        CallbackCompletion::Cancel
                    } else {
                        CallbackCompletion::DrainNotifications
                    };
                    finish_callbacks(
                        &callback_cancellation_token,
                        &mut notification_tasks,
                        &mut tool_tasks,
                        callback_completion,
                        task_failure_handler.as_ref(),
                    )
                    .await;
                    let (workflow_error, runtime_error) = if journal_failed {
                        (
                            WORKFLOW_JOURNAL_UNAVAILABLE_ERROR,
                            WORKFLOW_JOURNAL_UNAVAILABLE_ERROR,
                        )
                    } else {
                        (
                            "workflow runtime ended unexpectedly",
                            "exec runtime ended unexpectedly",
                        )
                    };
                    report_workflow_completion(
                        host.as_ref(),
                        workflow,
                        WorkflowHostCompletion::Errored(workflow_error.to_string()),
                    )
                    .await;
                    let event = CellEvent::Completed {
                        content_items: std::mem::take(&mut content_items),
                        error_text: Some(runtime_error.to_string()),
                    };
                    let rejected_event = match host
                        .commit_completion(
                            HashMap::new(),
                            event,
                            /*pending_initial_yield_items*/ None,
                            Arc::clone(&cell_state),
                        )
                        .await
                    {
                        CompletionCommit::Committed => None,
                        CompletionCommit::Rejected(event) => Some(event),
                    };
                    match cell_state.deliver_completion(
                        observer.take().map(|observer| observer.response_tx),
                    ) {
                        CompletionDelivery::Delivered => break,
                        CompletionDelivery::Buffered => {}
                        CompletionDelivery::Rejected(response_tx) => {
                            finish_termination(
                                &cell_state,
                                response_tx,
                                CellEvent::Terminated {
                                    content_items: rejected_completion_content(rejected_event),
                                },
                            );
                            break;
                        }
                    }
                    continue;
                };
                if journal_failed {
                    continue;
                }
                match event {
                    RuntimeEvent::Started => {
                        yield_timer = observer.as_ref().and_then(observer_timer);
                    }
                    RuntimeEvent::Pending => {
                        runtime_paused = true;
                        if matches!(
                            observer.as_ref().map(|observer| observer.mode),
                            Some(ObserveMode::PendingFrontier)
                        ) {
                            yield_timer = None;
                            pending_frontier_ready = false;
                            match send_observer_event(
                                observer.take(),
                                CellEvent::Pending {
                                    content_items: std::mem::take(&mut content_items),
                                    pending_tool_call_ids: std::mem::take(
                                        &mut pending_tool_call_ids,
                                    ),
                                },
                            ) {
                                Ok(()) => {}
                                Err(CellEvent::Pending {
                                    content_items: undelivered_items,
                                    pending_tool_call_ids: undelivered_tool_call_ids,
                                }) => {
                                    content_items = undelivered_items;
                                    pending_tool_call_ids = undelivered_tool_call_ids;
                                    pending_frontier_ready = true;
                                }
                                Err(event) => {
                                    panic!("pending delivery returned an unexpected event: {event:?}")
                                }
                            }
                        } else {
                            pending_tool_call_ids.clear();
                            let _ = runtime_control_tx.send(RuntimeControlCommand::Continue);
                            runtime_paused = false;
                        }
                    }
                    RuntimeEvent::ContentItem(item) => content_items.push(output_item(item)),
                    RuntimeEvent::YieldRequested => {
                        let yield_observer = matches!(
                            observer.as_ref().map(|observer| observer.mode),
                            Some(ObserveMode::YieldAfter(_))
                        );
                        if yield_observer {
                            yield_timer = None;
                            restore_undelivered_yield(
                                send_observer_event(
                                    observer.take(),
                                    CellEvent::Yielded {
                                        content_items: std::mem::take(&mut content_items),
                                    },
                                ),
                                &mut content_items,
                            );
                        }
                    }
                    // Workflow narrator events. The protocol `Workflow*` event
                    // cluster + app-server mapping are a later ticket, but the
                    // markers are journaled now (§7 `phase`/`log` lines) so the
                    // run tree reconstructs from `journal.jsonl` on resume. The
                    // host routes these to the run's recorder (a no-op for a
                    // non-journaled cell). Awaited inline so the line is durable
                    // and ordered relative to the surrounding agent-call lines.
                    // A failed acknowledgement stops the workflow because
                    // continuing would make its replay history incomplete.
                    RuntimeEvent::Phase { title } => {
                        if let Err(error) = host.journal_phase(title).await {
                            warn!(error = %error, "failed to persist workflow phase journal record");
                            journal_failed = true;
                            stop_runtime(
                                &runtime_tx,
                                &runtime_control_tx,
                                &runtime_terminate_handle,
                            );
                        }
                    }
                    RuntimeEvent::WorkflowLog { message } => {
                        if let Err(error) = host.journal_log(message).await {
                            warn!(error = %error, "failed to persist workflow log journal record");
                            journal_failed = true;
                            stop_runtime(
                                &runtime_tx,
                                &runtime_control_tx,
                                &runtime_terminate_handle,
                            );
                        }
                    }
                    // Workflow `agent()` spawn requests. Mirrors the `ToolCall`
                    // path: spawn one independent task into the shared tool
                    // JoinSet that routes the call through the host to the spawn
                    // helper and settles the isolate promise by id via the same
                    // resolve/reject commands a nested tool uses. The host's
                    // `AgentSpawnOutcome` maps to `ToolResponse` (`Completed` -> JS
                    // value, `Failed` -> JS null; `agent()` never throws for agent
                    // failure) or `ToolError` (`Rejected` -> the isolate throws the
                    // cap/budget message). The tasks do not touch the consume loop's
                    // state, so N concurrent `agent()` calls resolve independently
                    // and out-of-order.
                    RuntimeEvent::AgentCall {
                        id,
                        node_id,
                        parent_node_id,
                        phase,
                        ordinal,
                        prompt,
                        opts,
                    } => {
                        spawn_agent(
                            &mut tool_tasks,
                            Arc::clone(&host),
                            id,
                            node_id,
                            parent_node_id,
                            phase,
                            prompt,
                            ordinal,
                            opts,
                            budget.clone(),
                            runtime_tx.clone(),
                            callback_cancellation_token.child_token(),
                            task_failure_handler.clone(),
                        );
                    }
                    // Prefix-replay cache hit (§7 resume step 3): the isolate
                    // matched this ordinal's recomputed key against the journaled
                    // entry, so NO subagent is spawned. Route the entry to the host
                    // inline (awaited before the next event is drained) so its
                    // `tokens_spent` is charged to this run's workflow meter and it is
                    // re-appended to the NEW run's journal BEFORE any later
                    // divergent live `agent()` runs its pre-admission budget check —
                    // that is what makes the ceiling throw land at the identical
                    // ordinal as the original run. Only a successful acknowledgement
                    // settles the isolate promise with the cached response. A failed
                    // acknowledgement is fatal: letting workflow code catch it would permit
                    // execution to continue from an incomplete durable prefix.
                    RuntimeEvent::AgentReplay {
                        id,
                        node_id,
                        parent_node_id,
                        phase,
                        entry,
                    } => {
                        let result = entry.ret.clone();
                        match host
                            .replay_agent(node_id, parent_node_id, phase, *entry)
                            .await
                        {
                            Ok(()) => {
                                refresh_workflow_budget(host.as_ref(), budget.as_ref()).await;
                                let _ = runtime_tx.send(RuntimeCommand::ToolResponse { id, result });
                            }
                            Err(error) => {
                                warn!(
                                    error = %error,
                                    "failed to persist workflow replay journal record"
                                );
                                journal_failed = true;
                                stop_runtime(
                                    &runtime_tx,
                                    &runtime_control_tx,
                                    &runtime_terminate_handle,
                                );
                            }
                        }
                    }
                    RuntimeEvent::WorkflowProgress(event) => {
                        host.workflow_progress(WorkflowHostProgress::Event { event })
                            .await;
                    }
                    // Workflow `workflow(nameOrRef, args)` nested-run requests.
                    // Mirrors the `AgentCall` path: spawn one independent task that
                    // routes the call through the host, which resolves the named
                    // saved workflow from the registry, re-enters the runtime one
                    // level deep, and settles the isolate promise by id via the
                    // same resolve/reject commands a nested tool uses. The host's
                    // `AgentSpawnOutcome` maps to `ToolResponse` (`Completed` -> the
                    // nested run's result, `Failed` -> JS null) or `ToolError`
                    // (`Rejected` -> the isolate throws, e.g. an unresolved name).
                    RuntimeEvent::WorkflowCall { id, name, args } => {
                        spawn_workflow(
                            &mut tool_tasks,
                            Arc::clone(&host),
                            id,
                            name,
                            args,
                            budget.clone(),
                            runtime_tx.clone(),
                            callback_cancellation_token.child_token(),
                            task_failure_handler.clone(),
                        );
                    }
                    RuntimeEvent::Notify { call_id, text } => {
                        spawn_notification(
                            &mut notification_tasks,
                            Arc::clone(&host),
                            call_id,
                            text,
                            callback_cancellation_token.child_token(),
                            task_failure_handler.clone(),
                        );
                    }
                    RuntimeEvent::ToolCall { id, name, kind, input } => {
                        pending_tool_call_ids.push(id.clone());
                        spawn_tool(
                            &mut tool_tasks,
                            Arc::clone(&host),
                            CellToolCall {
                                id,
                                name: CellToolName {
                                    name: name.name,
                                    namespace: name.namespace,
                                },
                                kind: cell_tool_kind(kind),
                                input,
                            },
                            runtime_tx.clone(),
                            callback_cancellation_token.child_token(),
                            task_failure_handler.clone(),
                        );
                    }
                    RuntimeEvent::Result { stored_value_writes, error_text } => {
                        runtime_closed = true;
                        yield_timer = None;
                        if termination || cancellation_token.is_cancelled() {
                            finish_callbacks(
                                &callback_cancellation_token,
                                &mut notification_tasks,
                                &mut tool_tasks,
                                CallbackCompletion::Cancel,
                                task_failure_handler.as_ref(),
                            ).await;
                            report_workflow_completion(
                                host.as_ref(),
                                workflow,
                                WorkflowHostCompletion::Interrupted,
                            )
                            .await;
                            finish_termination(
                                &cell_state,
                                observer.take().map(|observer| observer.response_tx),
                                CellEvent::Terminated {
                                    content_items: std::mem::take(&mut content_items),
                                },
                            );
                            break;
                        }
                        finish_callbacks(
                            &callback_cancellation_token,
                            &mut notification_tasks,
                            &mut tool_tasks,
                            CallbackCompletion::DrainNotifications,
                            task_failure_handler.as_ref(),
                        )
                        .await;
                        let completion = match error_text.as_deref() {
                            Some(error) => WorkflowHostCompletion::Errored(
                                bound_workflow_progress_error(error),
                            ),
                            None => WorkflowHostCompletion::Completed,
                        };
                        report_workflow_completion(host.as_ref(), workflow, completion).await;
                        let event = CellEvent::Completed {
                            content_items: std::mem::take(&mut content_items),
                            error_text,
                        };
                        let rejected_event = match host
                            .commit_completion(
                                stored_value_writes,
                                event,
                                /*pending_initial_yield_items*/ None,
                                Arc::clone(&cell_state),
                            )
                            .await
                        {
                            CompletionCommit::Committed => None,
                            CompletionCommit::Rejected(event) => Some(event),
                        };
                        match cell_state.deliver_completion(
                            observer.take().map(|observer| observer.response_tx),
                        ) {
                            CompletionDelivery::Delivered => break,
                            CompletionDelivery::Buffered => {}
                            CompletionDelivery::Rejected(response_tx) => {
                                finish_termination(
                                    &cell_state,
                                    response_tx,
                                    CellEvent::Terminated {
                                        content_items: rejected_completion_content(rejected_event),
                                    },
                                );
                                break;
                            }
                        }
                    }
                    RuntimeEvent::ThreadPanicked => {
                        runtime_failure_reported = true;
                    }
                }
            }
            task_result = notification_tasks.join_next(), if !notification_tasks.is_empty() => {
                report_task_result(
                    task_result,
                    "notification",
                    task_failure_handler.as_ref(),
                );
            }
            task_result = tool_tasks.join_next(), if !tool_tasks.is_empty() => {
                report_task_result(task_result, "tool", task_failure_handler.as_ref());
            }
        }
    }
    // Reject requests that arrive while asynchronous terminal cleanup runs.
    cell_state.tombstone();
    drop(command_rx.take());
    begin_termination(
        &runtime_tx,
        &runtime_control_tx,
        &runtime_terminate_handle,
        &cancellation_token,
    );
    finish_callbacks(
        &callback_cancellation_token,
        &mut notification_tasks,
        &mut tool_tasks,
        CallbackCompletion::Cancel,
        task_failure_handler.as_ref(),
    )
    .await;
    host.closed().await;
}

async fn report_workflow_completion<H: CellHost>(
    host: &H,
    workflow: bool,
    status: WorkflowHostCompletion,
) {
    if workflow {
        host.workflow_progress(WorkflowHostProgress::Complete { status })
            .await;
    }
}

fn bound_workflow_progress_error(error: &str) -> String {
    const MAX_BYTES: usize = 2048;
    const MARKER: &str = "… [error truncated]";
    if error.len() <= MAX_BYTES {
        return error.to_string();
    }
    let mut end = MAX_BYTES.saturating_sub(MARKER.len());
    while end > 0 && !error.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &error[..end])
}

fn send_observer_event(observer: Option<Observer>, event: CellEvent) -> Result<(), CellEvent> {
    let Some(observer) = observer else {
        return Err(event);
    };
    send_cell_event(observer.response_tx, event)
}

fn send_cell_event(
    response_tx: oneshot::Sender<Result<CellEvent, CellError>>,
    event: CellEvent,
) -> Result<(), CellEvent> {
    match response_tx.send(Ok(event)) {
        Ok(()) => Ok(()),
        Err(Ok(event)) => Err(event),
        Err(Err(error)) => panic!("cell event delivery returned an actor error: {error:?}"),
    }
}

fn restore_undelivered_yield(delivery: Result<(), CellEvent>, content_items: &mut Vec<OutputItem>) {
    match delivery {
        Ok(()) => {}
        Err(CellEvent::Yielded {
            content_items: mut undelivered_items,
        }) => {
            undelivered_items.append(content_items);
            *content_items = undelivered_items;
        }
        Err(event) => panic!("yield delivery returned an unexpected event: {event:?}"),
    }
}

fn rejected_completion_content(event: Option<CellEvent>) -> Vec<OutputItem> {
    match event {
        Some(CellEvent::Completed { content_items, .. }) => content_items,
        None => Vec::new(),
        Some(event) => panic!("completion commit rejected an unexpected event: {event:?}"),
    }
}

fn finish_termination(
    cell_state: &CellState,
    observer_tx: Option<oneshot::Sender<Result<CellEvent, CellError>>>,
    event: CellEvent,
) {
    if let Some(event) = cell_state.finish_termination(event)
        && let Some(observer_tx) = observer_tx
    {
        let _ = observer_tx.send(Ok(event));
    }
}

fn observer_timer(observer: &Observer) -> Option<std::pin::Pin<Box<tokio::time::Sleep>>> {
    match observer.mode {
        ObserveMode::YieldAfter(duration) => Some(Box::pin(tokio::time::sleep(duration))),
        ObserveMode::PendingFrontier => None,
    }
}

fn resume_for_observation(
    mode: ObserveMode,
    runtime_paused: &mut bool,
    runtime_tx: &std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_control_tx: &std::sync::mpsc::Sender<RuntimeControlCommand>,
) {
    if *runtime_paused {
        let control = match mode {
            ObserveMode::YieldAfter(_) => RuntimeControlCommand::Continue,
            ObserveMode::PendingFrontier => RuntimeControlCommand::Resume,
        };
        let _ = runtime_control_tx.send(control);
        *runtime_paused = false;
    } else if matches!(mode, ObserveMode::PendingFrontier) {
        let _ = runtime_tx.send(RuntimeCommand::ObservePendingFrontier);
    }
}

fn begin_termination(
    runtime_tx: &std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_control_tx: &std::sync::mpsc::Sender<RuntimeControlCommand>,
    runtime_terminate_handle: &v8::IsolateHandle,
    cancellation_token: &CancellationToken,
) {
    cancellation_token.cancel();
    stop_runtime(runtime_tx, runtime_control_tx, runtime_terminate_handle);
}

fn stop_runtime(
    runtime_tx: &std::sync::mpsc::Sender<RuntimeCommand>,
    runtime_control_tx: &std::sync::mpsc::Sender<RuntimeControlCommand>,
    runtime_terminate_handle: &v8::IsolateHandle,
) {
    let _ = runtime_tx.send(RuntimeCommand::Terminate);
    let _ = runtime_control_tx.send(RuntimeControlCommand::Terminate);
    let _ = runtime_terminate_handle.terminate_execution();
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

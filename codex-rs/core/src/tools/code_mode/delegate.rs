use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use super::workflow_handler::WorkflowRunLedger;
use super::workflow_handler::run_workflow_by_name;
use codex_code_mode::AgentCallOpts;
use codex_code_mode::AgentSpawnFuture;
use codex_code_mode::AgentSpawnOutcome;
use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSessionDelegate;
use codex_code_mode::NotificationFuture;
use codex_code_mode::ToolInvocationFuture;
use codex_code_mode::WorkflowBudgetHandle;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_protocol::protocol::ThreadGoalUpdatedEvent;
use codex_protocol::user_input::UserInput;
use codex_workflow_journal::AgentCallLine;
use codex_workflow_journal::AgentCallOpts as JournalAgentCallOpts;
use codex_workflow_journal::AgentStatus as JournalAgentStatus;
use codex_workflow_journal::JournalRecorder;
use codex_workflow_journal::KeyInputs;
use codex_workflow_journal::LogLine;
use codex_workflow_journal::NullOrdinal;
use codex_workflow_journal::PhaseLine;
use codex_workflow_journal::prompt_hash;
use codex_workflow_journal::schema_hash;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use super::ExecContext;
use super::PUBLIC_TOOL_NAME;
use super::call_nested_tool;
use super::scheduler::AgentCapReached;
use super::scheduler::SpawnAttempt;
use super::scheduler::WorkflowScheduler;
use crate::agent::control::SpawnAgentOptions;
use crate::agent::control::spawn_await::workflow_agent_nickname_preference;
use crate::agent::control::spawn_await_opts::SpawnAgentConfigOverrides;
use crate::rollout_budget::RolloutBudget;
use crate::rollout_budget::RolloutBudgetHandle;
use crate::session::step_context::StepContext;
use crate::tools::ToolRouter;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::parallel::ToolCallRuntime;

pub(super) struct CodeModeDispatchBroker {
    dispatch_tx: async_channel::Sender<DispatchMessage>,
    dispatch_rx: async_channel::Receiver<DispatchMessage>,
    dispatch_gates: Arc<Mutex<HashMap<CellId, watch::Sender<bool>>>>,
    /// Shared run→parent ledger, forgotten per closed cell (see [`WorkflowRunLedger`]).
    workflow_run_ledger: Arc<WorkflowRunLedger>,
    /// The workflow session's shared, tree-wide [`RolloutBudget`], captured from the
    /// session the first time a turn worker starts (SEAM #1). The broker is 1:1 with
    /// one `CodeModeService` on one `Session`, and that session's `AgentControl` holds
    /// a single budget `Arc` for its whole lifetime (reconfigured per-run through the
    /// resettable cell, never swapped), so a single captured `Arc` is always the
    /// correct run's budget and its getters read live — never a stale/other session's
    /// counter. Backs the in-process `budget_handle()` (§4/§8) so `budget.spent()` /
    /// `budget.remaining()` forward to live spend instead of static defaults.
    workflow_budget: std::sync::OnceLock<Arc<RolloutBudget>>,
    /// Prefix-replay seed staged by the resume entrypoint (`P3-resume-entry`, spec §7
    /// steps 1-3): the prior run's loaded `agent_call` journal lines, serialized as
    /// JSON so this is a plain data hand-off. Set via [`stage_replay_entries`] just
    /// before the resumed top-level run's `service.execute`, then consumed exactly
    /// ONCE — by the first cell the runtime spawns, which is that top-level run
    /// ([`replay_entries`]). Later nested `workflow()` cells find it empty and run
    /// fresh. `None`/empty for every non-resume run, so a live run seeds no replay.
    ///
    /// [`stage_replay_entries`]: CodeModeDispatchBroker::stage_replay_entries
    /// [`replay_entries`]: codex_code_mode_protocol::CodeModeSessionDelegate::replay_entries
    pending_replay: Mutex<Option<Vec<JsonValue>>>,
}

impl CodeModeDispatchBroker {
    pub(super) fn new(workflow_run_ledger: Arc<WorkflowRunLedger>) -> Self {
        let (dispatch_tx, dispatch_rx) = async_channel::unbounded();
        Self {
            dispatch_tx,
            dispatch_rx,
            dispatch_gates: Arc::new(Mutex::new(HashMap::new())),
            workflow_run_ledger,
            workflow_budget: std::sync::OnceLock::new(),
            pending_replay: Mutex::new(None),
        }
    }

    /// Stage the prior run's journal `agent_call` lines as the prefix-replay seed for
    /// the next cell the runtime spawns (the resumed top-level run). Must be called
    /// immediately before that run's `service.execute` so the top-level cell — never a
    /// nested `workflow()` cell — is the one that consumes it. An empty `entries`
    /// clears any prior staging (a no-op seed). See [`Self::pending_replay`].
    pub(super) fn stage_replay_entries(&self, entries: Vec<JsonValue>) {
        let staged = if entries.is_empty() {
            None
        } else {
            Some(entries)
        };
        if let Ok(mut slot) = self.pending_replay.lock() {
            *slot = staged;
        }
    }

    pub(super) fn mark_cell_ready_for_dispatch(&self, cell_id: &CellId) {
        dispatch_gate(&self.dispatch_gates, cell_id).send_replace(true);
    }

    pub(super) fn close_cell(&self, cell_id: &CellId) {
        remove_dispatch_gate(&self.dispatch_gates, cell_id);
        // Drop the cell→run_id entry once its cell is gone so the ledger map does
        // not grow across a long session; the append-only run→parent links stay.
        self.workflow_run_ledger.forget_cell(cell_id);
    }

    pub(super) fn start_turn_worker(
        &self,
        exec: ExecContext,
        router: Arc<ToolRouter>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> CodeModeDispatchWorker {
        // Capture this session's shared budget `Arc` (SEAM #1) before `exec` is moved
        // into the host. `set` is idempotent: every turn worker on this session shares
        // the same budget `Arc`, so a repeat call is a no-op and never installs a
        // different (stale) session's counter.
        let _ = self
            .workflow_budget
            .set(exec.session.services.agent_control.rollout_budget_arc());
        let tool_runtime =
            ToolCallRuntime::new(router, Arc::clone(&exec.session), step_context, tracker);
        // One `WorkflowScheduler` per turn worker (i.e. per workflow run): every `agent()` call in
        // the run dispatches to this single `CoreTurnHost`, so its scheduler is what bounds the
        // whole `parallel()`/`pipeline()` fan-out's concurrency and enforces the per-run lifetime
        // cap (spec §5). Cap = `min(16, cores-2)` clamped by the parent turn's
        // `effective_agent_max_threads` (raised to the workflow ceiling).
        let scheduler = WorkflowScheduler::new(
            exec.turn
                .config
                .effective_agent_max_threads(exec.turn.multi_agent_version),
        );
        let host = Arc::new(CoreTurnHost {
            exec,
            tool_runtime,
            scheduler,
        });
        let dispatch_rx = self.dispatch_rx.clone();
        let dispatch_gates = Arc::clone(&self.dispatch_gates);
        let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
        tokio::spawn(async move {
            loop {
                let message = tokio::select! {
                    _ = &mut shutdown_rx => break,
                    message = dispatch_rx.recv() => message.ok(),
                };
                let Some(message) = message else {
                    break;
                };
                match message {
                    DispatchMessage::Notify {
                        call_id,
                        cell_id,
                        text,
                        cancellation_token,
                        response_tx,
                    } => {
                        let response = if wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            host.notify(call_id, cell_id, text).await
                        } else {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            Err("code mode notification cancelled".to_string())
                        };
                        let _ = response_tx.send(response);
                    }
                    DispatchMessage::InvokeTool {
                        invocation,
                        cancellation_token,
                        response_tx,
                    } => {
                        let cell_id = invocation.cell_id.clone();
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            continue;
                        }
                        let host = Arc::clone(&host);
                        tokio::spawn(async move {
                            let response = tokio::select! {
                                response = host.invoke_tool(
                                    invocation,
                                    cancellation_token.clone(),
                                ) => response,
                                _ = cancellation_token.cancelled() => return,
                            };
                            let _ = response_tx.send(response);
                        });
                    }
                    DispatchMessage::SpawnAgent {
                        cell_id,
                        prompt,
                        ordinal,
                        opts,
                        cancellation_token,
                        response_tx,
                    } => {
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            continue;
                        }
                        // One independent task per `agent()` call: N concurrent calls run through the
                        // spawn helper concurrently and resolve out-of-order, with nothing in the
                        // dispatch loop serializing them (the loop only enqueues).
                        let host = Arc::clone(&host);
                        tokio::spawn(async move {
                            let result = tokio::select! {
                                result = host.spawn_agent(cell_id, prompt, ordinal, opts) => result,
                                _ = cancellation_token.cancelled() => return,
                            };
                            let _ = response_tx.send(result);
                        });
                    }
                    DispatchMessage::SpawnWorkflow {
                        cell_id,
                        name,
                        args,
                        cancellation_token,
                        response_tx,
                    } => {
                        if !wait_until_cell_ready_for_dispatch(
                            &dispatch_gates,
                            &cell_id,
                            &cancellation_token,
                        )
                        .await
                        {
                            remove_dispatch_gate(&dispatch_gates, &cell_id);
                            continue;
                        }
                        // One independent task per `workflow()` call: the nested run
                        // re-enters the runtime through this same host, and its own
                        // agents dispatch back into this loop, so nothing here may
                        // block the loop while it runs.
                        let host = Arc::clone(&host);
                        tokio::spawn(async move {
                            let result = tokio::select! {
                                result = host.spawn_workflow(cell_id, name, args) => result,
                                _ = cancellation_token.cancelled() => return,
                            };
                            let _ = response_tx.send(result);
                        });
                    }
                }
            }
        });
        CodeModeDispatchWorker {
            shutdown_tx: Some(shutdown_tx),
        }
    }
}

fn dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, watch::Sender<bool>>>,
    cell_id: &CellId,
) -> watch::Sender<bool> {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates
        .entry(cell_id.clone())
        .or_insert_with(|| watch::channel(false).0)
        .clone()
}

fn remove_dispatch_gate(
    dispatch_gates: &Mutex<HashMap<CellId, watch::Sender<bool>>>,
    cell_id: &CellId,
) {
    let mut dispatch_gates = match dispatch_gates.lock() {
        Ok(dispatch_gates) => dispatch_gates,
        Err(poisoned) => poisoned.into_inner(),
    };
    dispatch_gates.remove(cell_id);
}

async fn wait_until_cell_ready_for_dispatch(
    dispatch_gates: &Mutex<HashMap<CellId, watch::Sender<bool>>>,
    cell_id: &CellId,
    cancellation_token: &CancellationToken,
) -> bool {
    if cancellation_token.is_cancelled() {
        return false;
    }
    let mut ready_rx = dispatch_gate(dispatch_gates, cell_id).subscribe();
    loop {
        if *ready_rx.borrow_and_update() {
            return true;
        }
        tokio::select! {
            changed = ready_rx.changed() => {
                if changed.is_err() {
                    return false;
                }
            }
            _ = cancellation_token.cancelled() => return false,
        }
    }
}

impl CodeModeSessionDelegate for CodeModeDispatchBroker {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode nested tool call cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::InvokeTool {
                    invocation,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode nested tool dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode nested tool dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode nested tool call cancelled".to_string())
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a> {
        Box::pin(async move {
            if cancellation_token.is_cancelled() {
                return Err("code mode notification cancelled".to_string());
            }
            let (response_tx, response_rx) = oneshot::channel();
            self.dispatch_tx
                .send(DispatchMessage::Notify {
                    call_id,
                    cell_id,
                    text,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .map_err(|_| "code mode notification dispatcher is unavailable".to_string())?;
            tokio::select! {
                response = response_rx => response
                    .map_err(|_| "code mode notification dispatcher stopped".to_string())?,
                _ = cancellation_token.cancelled() => {
                    Err("code mode notification cancelled".to_string())
                }
            }
        })
    }

    fn spawn_agent<'a>(
        &'a self,
        cell_id: CellId,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        Box::pin(async move {
            // A cancelled call or an unavailable/stopped dispatcher resolves the isolate promise to
            // JS `null` (death-is-null) rather than throwing — only an admission-time cap rejection
            // (surfaced by the host as `AgentSpawnOutcome::Rejected`) throws.
            if cancellation_token.is_cancelled() {
                return AgentSpawnOutcome::Failed;
            }
            let (response_tx, response_rx) = oneshot::channel();
            if self
                .dispatch_tx
                .send(DispatchMessage::SpawnAgent {
                    cell_id,
                    prompt,
                    ordinal,
                    opts,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .is_err()
            {
                return AgentSpawnOutcome::Failed;
            }
            tokio::select! {
                result = response_rx => result.unwrap_or(AgentSpawnOutcome::Failed),
                _ = cancellation_token.cancelled() => AgentSpawnOutcome::Failed,
            }
        })
    }

    fn spawn_workflow<'a>(
        &'a self,
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        Box::pin(async move {
            // A cancelled call or an unavailable/stopped dispatcher resolves the isolate promise to
            // JS `null` (death-is-null) rather than throwing; only an unresolved name / nested error
            // (surfaced by the host as `Rejected`) throws.
            if cancellation_token.is_cancelled() {
                return AgentSpawnOutcome::Failed;
            }
            let (response_tx, response_rx) = oneshot::channel();
            if self
                .dispatch_tx
                .send(DispatchMessage::SpawnWorkflow {
                    cell_id,
                    name,
                    args,
                    cancellation_token: cancellation_token.clone(),
                    response_tx,
                })
                .await
                .is_err()
            {
                return AgentSpawnOutcome::Failed;
            }
            tokio::select! {
                result = response_rx => result.unwrap_or(AgentSpawnOutcome::Failed),
                _ = cancellation_token.cancelled() => AgentSpawnOutcome::Failed,
            }
        })
    }

    fn budget_handle(&self) -> Option<Arc<dyn WorkflowBudgetHandle>> {
        // Return a LIVE handle over this session's shared, tree-wide budget so the
        // in-process code-mode isolate's `budget.spent()` / `budget.remaining()`
        // globals forward to the real counter (§4/§8) rather than the static
        // `spent 0 / remaining total` defaults. The handle reads the budget live at
        // call time, so a workflow that awaits subagents observes accrued spend. This
        // does NOT touch the hard-ceiling enforcement, which reads the `RolloutBudget`
        // directly in `CoreTurnHost::spawn_agent`. `None` before the first turn worker
        // starts falls back to the prior (static) behavior.
        self.workflow_budget.get().map(|budget| {
            Arc::new(RolloutBudgetHandle::new(Arc::clone(budget))) as Arc<dyn WorkflowBudgetHandle>
        })
    }

    fn replay_entries(&self, cell_id: CellId) -> Vec<JsonValue> {
        // Hand the staged prefix-replay seed (spec §7 steps 1-3, `P3-resume-entry`) to
        // the FIRST cell the runtime spawns after `stage_replay_entries` — the resumed
        // top-level run. `take` consumes it so a later nested `workflow()` cell finds
        // nothing staged and runs fresh (each nested run mints its own runId and is not
        // a resume). `cell_id` is unused because staging happens before the cell id is
        // known (the ledger's cell→run mapping is registered only after `execute`
        // returns); the one-shot consume order is what binds the seed to the top-level
        // run. A non-resume run stages nothing, so this returns empty and every
        // `agent()` dispatches live.
        let _ = cell_id;
        self.pending_replay
            .lock()
            .ok()
            .and_then(|mut slot| slot.take())
            .unwrap_or_default()
    }

    fn journal_phase<'a>(&'a self, cell_id: CellId, title: String) -> NotificationFuture<'a> {
        // Route the `phase(title)` marker to the run's journal (§7 `phase` line, ordinal always null).
        // A `None` recorder (plain code-mode exec / unregistered cell) is a no-op.
        Box::pin(async move {
            // `phase()` can fire at the very top of the script body, BEFORE
            // `run_workflow_source` registers this cell's recorder and opens the
            // dispatch gate. Resolving the recorder eagerly would race registration and
            // silently drop the line, so wait for the cell to be ready for dispatch
            // (the gate opens immediately AFTER `register_recorder`) exactly as the
            // agent-call path does, then resolve the recorder.
            wait_until_cell_ready_for_dispatch(
                &self.dispatch_gates,
                &cell_id,
                &CancellationToken::new(),
            )
            .await;
            if let Some(recorder) = self.workflow_run_ledger.recorder_for_cell(&cell_id) {
                recorder
                    .record_phase(PhaseLine {
                        timestamp: None,
                        ordinal: NullOrdinal,
                        title,
                    })
                    .await
                    .map_err(|err| err.to_string())?;
            }
            Ok(())
        })
    }

    fn journal_log<'a>(&'a self, cell_id: CellId, message: String) -> NotificationFuture<'a> {
        // Route the `log(message)` marker to the run's journal (§7 `log` line, ordinal always null).
        // Like `journal_phase`, wait for the recorder to be registered (dispatch gate
        // open) before resolving it so an early-body `log()` is not dropped.
        Box::pin(async move {
            wait_until_cell_ready_for_dispatch(
                &self.dispatch_gates,
                &cell_id,
                &CancellationToken::new(),
            )
            .await;
            if let Some(recorder) = self.workflow_run_ledger.recorder_for_cell(&cell_id) {
                recorder
                    .record_log(LogLine {
                        timestamp: None,
                        ordinal: NullOrdinal,
                        message,
                    })
                    .await
                    .map_err(|err| err.to_string())?;
            }
            Ok(())
        })
    }

    fn replay_agent<'a>(&'a self, cell_id: CellId, entry: JsonValue) -> NotificationFuture<'a> {
        // Prefix-replay cache hit (spec §7 "Resume algorithm" step 3): the isolate matched this
        // ordinal's recomputed `(prompt, opts)` key against the journaled entry and served the
        // promise from its `return` WITHOUT spawning. Two host-side effects reproduce the original
        // run byte-for-byte:
        //
        //  1. Re-add the journaled `tokens_spent` to the shared, tree-wide budget via the
        //     replay-only `add_spent` (`P3-budget-readd`) so `spent()`/`remaining()` and the
        //     pre-admission ceiling throw land at the identical ordinal as the original run
        //     (spec §8 "Resume determinism of budget"). This happens FIRST and synchronously with
        //     respect to the cell actor's serialized event drain, so a later divergent live
        //     `agent()` observes the replayed spend before its own pre-admission check.
        //  2. Re-append the entry to the NEW run's journal so the resumed run's `journal.jsonl`
        //     records the replayed prefix and is itself resumable (§7). A `None` recorder (plain
        //     code-mode exec / unregistered cell) skips the append.
        //
        // The entry crosses the wire-shaped protocol seam as JSON (see the trait doc); parse it back
        // into a typed line here. A record that fails to parse is dropped best-effort — it must not
        // disturb the run — matching the discard-on-failure policy of the journal markers above.
        let recorder = self.workflow_run_ledger.recorder_for_cell(&cell_id);
        let line: Option<AgentCallLine> = serde_json::from_value(entry).ok();
        if let Some(line) = line.as_ref()
            && let Some(tokens) = line.tokens_spent
            && let Some(budget) = self.workflow_budget.get()
        {
            budget.add_spent(tokens as i64);
        }
        Box::pin(async move {
            if let (Some(recorder), Some(line)) = (recorder, line) {
                recorder
                    .record_agent_call(line)
                    .await
                    .map_err(|err| err.to_string())?;
            }
            Ok(())
        })
    }

    fn cell_closed(&self, cell_id: &CellId) {
        self.close_cell(cell_id);
    }
}

enum DispatchMessage {
    InvokeTool {
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<JsonValue, String>>,
    },
    Notify {
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
        response_tx: oneshot::Sender<Result<(), String>>,
    },
    SpawnAgent {
        cell_id: CellId,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        cancellation_token: CancellationToken,
        // Three-way [`AgentSpawnOutcome`] (SEAM CONTRACT): `Completed(value)` on success (a JSON
        // string when schemaless, or the validated `opts.schema` object), `Failed` on agent
        // death/abort/parse-fail (JS null), and `Rejected(msg)` when a scheduler admission cap or a
        // bounds check refuses the spawn (a JS throw once the seam lands).
        response_tx: oneshot::Sender<AgentSpawnOutcome>,
    },
    SpawnWorkflow {
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
        // Reuses the [`AgentSpawnOutcome`] seam: `Completed(value)` -> the nested run's top-level
        // result, `Failed` -> JS null, `Rejected(msg)` -> a JS throw (unresolved name / nested error).
        response_tx: oneshot::Sender<AgentSpawnOutcome>,
    },
}

pub(crate) struct CodeModeDispatchWorker {
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl Drop for CodeModeDispatchWorker {
    fn drop(&mut self) {
        if let Some(shutdown_tx) = self.shutdown_tx.take() {
            let _ = shutdown_tx.send(());
        }
    }
}

/// Hard byte ceiling on the incoming `agent()` prompt before it becomes child context. A workflow
/// body can build an arbitrarily large prompt string in JS; without a bound it would flow verbatim
/// into the child's first-turn `UserInput`, blowing the per-agent ~10K-token individual-context
/// budget (spec §6). 64 KiB (~16K tokens) is a generous ceiling above which the prompt is truncated
/// with a marker rather than failing the call.
const WORKFLOW_PROMPT_MAX_BYTES: usize = 64 * 1024;

/// Hard ceiling on the serialized byte size of an `agent()` `opts.schema`. The schema is copied into
/// every child prompt and recompiled on each return; an unbounded one is both a context-budget and a
/// CPU DoS. 32 KiB is well above any legitimate structured-output schema.
const WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES: usize = 32 * 1024;

/// Hard ceiling on `opts.schema` nesting depth. `serde_json` already caps deserialization recursion,
/// but the schema is re-walked/compiled on every return, so an adversarially deep schema is bounded
/// here before use. 64 levels is far beyond any real JSON Schema.
const WORKFLOW_SCHEMA_MAX_DEPTH: usize = 64;

/// Conservative per-turn weighted-output estimate reserved against the budget when a workflow
/// `agent()` child is admitted (see [`crate::rollout_budget::RolloutBudget::reserve`]).
///
/// The reservation's job is to make concurrent admissions SERIALIZE against the ceiling; because a
/// reservation is scoped to a concurrency-permit holder, the "at most one in-flight turn per
/// concurrency slot" overshoot bound holds for any positive estimate. A modest per-turn floor keeps
/// budgeted parallelism unrestricted until the shared budget is nearly exhausted, at which point new
/// admissions are refused.
const WORKFLOW_AGENT_TURN_TOKEN_ESTIMATE: i64 = 1_024;

/// Per-`agent()`-call journal context (§7 write integration): the invariant fields (ordinal, cache
/// key, prompt hash, canonicalized opts, phase/label) computed once from the invocation, plus the
/// run's [`JournalRecorder`] (`None` for a non-journaled run). Wrapped in an `Arc` and cloned into the
/// admission closure so each terminal outcome appends exactly one `agent_call` line.
struct AgentCallJournalCtx {
    /// The run's journal writer, or `None` for plain code-mode exec (no run / no journal).
    recorder: Option<Arc<JournalRecorder>>,
    /// Invocation ordinal — the spine of prefix-replay (§7).
    ordinal: u64,
    /// `blake3:` cache key over the canonical `(prompt, opts)` (label/phase excluded).
    key: String,
    /// Content hash of the raw prompt text.
    prompt_hash: String,
    /// Canonicalized cache-relevant opts recorded on the line.
    opts: JournalAgentCallOpts,
    /// Progress-attribution phase (`opts.phase`), if any.
    phase: Option<String>,
    /// Progress-attribution label (`opts.label`), if any.
    label: Option<String>,
}

impl AgentCallJournalCtx {
    /// Append one `agent_call` line for this invocation's terminal outcome. A no-op when the run is
    /// not journaled. Best-effort: a write failure warns rather than failing the `agent()` call.
    async fn record(
        &self,
        status: Option<JournalAgentStatus>,
        ret: JsonValue,
        child_thread_id: Option<String>,
        rollout_path: Option<String>,
        tokens_spent: Option<u64>,
    ) {
        let Some(recorder) = self.recorder.as_ref() else {
            return;
        };
        let line = AgentCallLine {
            timestamp: None,
            ordinal: self.ordinal,
            key: self.key.clone(),
            prompt_hash: self.prompt_hash.clone(),
            opts: self.opts.clone(),
            phase: self.phase.clone(),
            label: self.label.clone(),
            child_thread_id,
            rollout_path,
            status,
            ret,
            tokens_spent,
            completion_seq: None,
        };
        if let Err(err) = recorder.record_agent_call(line).await {
            warn!(
                "failed to journal workflow agent() call (ordinal {}): {err}",
                self.ordinal
            );
        }
    }

    /// Record an `agent()` that THREW before (or instead of) spawning a child (a schema-bounds /
    /// budget / lifetime-cap rejection): `status:error`, `return:null`, no child linkage — which §7
    /// validation exempts from the completed-line linkage requirements.
    async fn record_error(&self) {
        self.record(
            Some(JournalAgentStatus::Error),
            JsonValue::Null,
            None,
            None,
            None,
        )
        .await;
    }
}

struct CoreTurnHost {
    exec: ExecContext,
    tool_runtime: ToolCallRuntime,
    /// Per-run concurrency + lifetime scheduler shared by every `agent()` call in this workflow run
    /// (spec §5). Constructed once in `start_turn_worker`; `admit` bounds concurrent spawns and
    /// enforces the monotonic lifetime cap.
    scheduler: WorkflowScheduler,
}

impl CoreTurnHost {
    async fn invoke_tool(
        &self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> Result<JsonValue, String> {
        call_nested_tool(
            self.exec.clone(),
            self.tool_runtime.clone(),
            invocation,
            cancellation_token,
        )
        .await
        .map_err(|error| error.to_string())
    }

    /// Route a workflow `agent(prompt, opts?)` call through the per-run [`WorkflowScheduler`] into
    /// the wave-2 spawn keystone
    /// [`crate::agent::control::AgentControl::spawn_and_await_final_message`], resolving to an
    /// [`AgentSpawnOutcome`].
    ///
    /// ## Admission (spec §5) & determinism
    ///
    /// Before spawning, the incoming prompt is byte-capped ([`cap_prompt_bytes`]) and any
    /// `opts.schema` is bounded ([`ensure_schema_within_bounds`]); an over-limit schema returns
    /// [`AgentSpawnOutcome::Rejected`] without consuming a lifetime slot. The call is then admitted
    /// through [`WorkflowScheduler::admit`]: the monotonic lifetime CAS runs first (over-cap ->
    /// `Rejected("AgentCapReached")`, no permit awaited), then a concurrency permit is held across
    /// the spawn and released on finalize on every path. The child nickname is derived purely from
    /// the invocation `ordinal` via [`workflow_agent_nickname_preference`] (no `rand`).
    ///
    /// The keystone constructs the `Subagent` source itself and drives the child's first turn to
    /// completion over the non-competing event tap. A normal final message becomes
    /// [`AgentSpawnOutcome::Completed`]; a dead/aborted child (or a config-build/spawn/submit
    /// failure, or a schema parse/validation failure) becomes [`AgentSpawnOutcome::Failed`] (JS
    /// null). `opts.model` / `opts.effort` / `opts.agentType` are threaded as
    /// [`SpawnAgentConfigOverrides`] and applied to the inherited child config before the spawn;
    /// omitted overrides inherit the parent turn.
    ///
    /// ## Structured output (`opts.schema`, spec §6)
    ///
    /// When `opts.schema` is present it is threaded onto the child's first turn as
    /// `final_output_json_schema`, forcing a StructuredOutput (`output_schema_strict = true`) final
    /// message. On return the raw final text is `serde_json`-parsed and, as **defense-in-depth**
    /// (engine strict mode is enforced for OpenAI providers but not guaranteed for all), re-validated
    /// against the JSON Schema with the `jsonschema` crate before the parsed object is resolved back
    /// to JS. A parse or validation failure resolves to `None` (JS `null`) per the death-is-null
    /// contract. Without `opts.schema` the plain final text is resolved as a JSON string. See
    /// [`finalize_agent_output`].
    async fn spawn_agent(
        &self,
        cell_id: CellId,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
    ) -> AgentSpawnOutcome {
        // Build the §7 journal context from the ordinal + the `(prompt, opts)` the isolate sent,
        // BEFORE any of `opts`/`prompt` is capped or moved below. The cache key and prompt hash are
        // computed from the RAW prompt the isolate hashed (never the byte-capped copy) so a resumed
        // run's recomputed key matches the journaled one; `label`/`phase` are recorded but excluded
        // from the key. The recorder is `None` for plain code-mode exec (no run, no journal).
        let journal = Arc::new(AgentCallJournalCtx {
            recorder: self
                .exec
                .session
                .services
                .code_mode_service
                .workflow_run_ledger()
                .recorder_for_cell(&cell_id),
            ordinal,
            key: KeyInputs {
                prompt: &prompt,
                model: opts.model.as_deref(),
                effort: opts.effort.as_deref(),
                agent_type: opts.agent_type.as_deref(),
                isolation: opts.isolation.as_deref(),
                schema: opts.schema.as_ref(),
            }
            .cache_key(),
            prompt_hash: prompt_hash(&prompt),
            opts: JournalAgentCallOpts {
                model: opts.model.clone(),
                effort: opts.effort.clone(),
                agent_type: opts.agent_type.clone(),
                isolation: opts.isolation.clone(),
                schema_hash: opts.schema.as_ref().map(schema_hash),
            },
            phase: opts.phase.clone(),
            label: opts.label.clone(),
        });

        // Bound the incoming `opts.schema` BEFORE it is threaded into any child prompt or recompiled
        // on return: an over-large / over-deep schema is a caller error, so REJECT the call with a
        // clear reason rather than admitting it and paying the context/CPU cost per child.
        if let Some(schema) = opts.schema.as_ref()
            && let Err(reason) = ensure_schema_within_bounds(schema)
        {
            warn!("workflow agent() rejected: {reason}");
            journal.record_error().await;
            return AgentSpawnOutcome::Rejected(reason);
        }
        // Bound the incoming prompt before it becomes child context (truncate with a marker so an
        // oversized prompt degrades gracefully rather than failing the whole call).
        let prompt = cap_prompt_bytes(prompt);

        let session = &self.exec.session;

        // Budget governance (spec §5 admission, §8). The shared, tree-wide `RolloutBudget` is shared
        // by the root thread and every cloned sub-agent control handle, so a workflow `budget.total`
        // ceiling meters output-token spend across the whole `parallel()`/`pipeline()` fan-out.
        //
        // Enforcement has two parts:
        //  1. A CHEAP, best-effort pre-check here that fast-rejects the steady-state exhausted case
        //     WITHOUT consuming a lifetime slot. `limit().is_some()` — not the old
        //     `spent()+remaining()>0` heuristic — is the authoritative metered signal: it treats an
        //     explicit zero ceiling (`Some(0)`) as metered while leaving an unconfigured budget
        //     (`None`) ungated. This read is racy under concurrency, so it is NOT the ceiling
        //     enforcer.
        //  2. The AUTHORITATIVE, race-free gate is an atomic reservation taken inside the
        //     concurrency-permit region below (`budget.reserve`): N concurrent admissions serialize
        //     on the shared lock, so they can no longer each observe headroom before any child
        //     records usage. See the reservation call site for the overshoot bound.
        //
        // A budget rejection is the one case `agent()` THROWS (surfaced as `Rejected` → a JS throw);
        // the death-is-null contract still governs agent death/abort.
        let budget = session.services.agent_control.rollout_budget();
        let turn_sub_id = self.exec.turn.sub_id.clone();
        // Reporting half of §8 governance: surface the current budget state on the EXISTING
        // ThreadGoal channel (no new protocol types). On an unmetered run this emits nothing.
        emit_budget_thread_goal(session, &turn_sub_id, budget).await;
        // Single source of truth for the pre-admission ceiling predicate (spec §5 step 1): the same
        // `RolloutBudget::pre_admission_rejects` the unit tests assert against, so the host gate and
        // its coverage can never drift.
        if budget.pre_admission_rejects() {
            warn!("workflow agent() rejected: BudgetExceeded (budget ceiling reached)");
            journal.record_error().await;
            return AgentSpawnOutcome::Rejected("BudgetExceeded".to_string());
        }

        let turn = self.exec.turn.as_ref();
        let scheduler = &self.scheduler;
        let schema = opts.schema;
        let overrides = SpawnAgentConfigOverrides {
            model: opts.model,
            effort: opts.effort,
            agent_type: opts.agent_type,
        };
        // Nickname is a pure function of the invocation ordinal (spec §7, no `rand`): the registry's
        // preferred-name branch reserves it verbatim (deterministically resolving any collision).
        let preferred_agent_nickname = Some(workflow_agent_nickname_preference(ordinal as usize));

        // Admit through the shared per-run scheduler (spec §5 admission order): the lifetime CAS
        // (step 2) runs first and rejects over-cap calls with `AgentCapReached` WITHOUT awaiting a
        // permit; then a concurrency permit is acquired (step 4) before the child is spawned, and
        // dropped on finalize on every path (step 6) via the permit RAII guard inside `admit`.
        let admit_result = scheduler
            .admit(|| {
                // Cloned per admission attempt (the scheduler may re-invoke on a registry-backstop
                // requeue); `session`/`turn`/`base_instructions` are cheap shared references.
                let prompt = prompt.clone();
                let schema = schema.clone();
                let overrides = overrides.clone();
                let preferred_agent_nickname = preferred_agent_nickname.clone();
                let turn_sub_id = turn_sub_id.clone();
                let journal = Arc::clone(&journal);
                async move {
                    // Authoritative, race-free budget gate (finding #1, spec §5/§8). Reserve one
                    // per-turn estimate under the shared lock so N concurrent admissions SERIALIZE
                    // against the ceiling instead of each reading headroom before any child records
                    // usage. The reservation is held only across THIS concurrency-permit region and
                    // released on finalize below, so at most `C` (the run's concurrency cap)
                    // reservations exist at once: the ceiling overshoots by at most one in-flight
                    // turn per concurrency slot — never by "up to N" for an N-wide fan-out.
                    let admission = budget.reserve(WORKFLOW_AGENT_TURN_TOKEN_ESTIMATE);
                    if matches!(admission, crate::rollout_budget::BudgetAdmission::Rejected) {
                        // Emit `BudgetLimited` on the crossing so a run that exhausts mid-fan-out is
                        // reported even without a further `agent()` attempt.
                        emit_budget_thread_goal(session, &turn_sub_id, budget).await;
                        warn!("workflow agent() rejected: BudgetExceeded (budget ceiling reached)");
                        journal.record_error().await;
                        return SpawnAttempt::Finalized(AgentSpawnOutcome::Rejected(
                            "BudgetExceeded".to_string(),
                        ));
                    }
                    let reserved =
                        matches!(admission, crate::rollout_budget::BudgetAdmission::Reserved);
                    let base_instructions = session.get_base_instructions().await;
                    let parent_thread_id = session.thread_id;
                    let options = SpawnAgentOptions {
                        parent_thread_id: Some(parent_thread_id),
                        environments: Some(turn.environments.to_selections()),
                        preferred_agent_nickname,
                        ..Default::default()
                    };
                    let spawn_outcome = session
                        .services
                        .agent_control
                        .spawn_and_await_journaled(
                            &base_instructions,
                            turn,
                            parent_thread_id,
                            vec![UserInput::Text {
                                text: prompt,
                                text_elements: Vec::new(),
                            }],
                            schema.clone(),
                            overrides,
                            options,
                        )
                        .await;
                    // A normal final message -> `Completed`; agent death/abort/schema parse-fail ->
                    // `Failed`. Both are terminal `Finalized` outcomes, so the permit releases either
                    // way (the registry-backstop `AgentLimitReached` requeue is a scheduler unit
                    // concern; the keystone maps a saturated-registry spawn error to `None` here).
                    let outcome =
                        match finalize_agent_output(spawn_outcome.final_text, schema.as_ref()) {
                            Some(value) => AgentSpawnOutcome::Completed(value),
                            None => AgentSpawnOutcome::Failed,
                        };
                    // Journal the finalized `agent()` at admission-order step 7 (§5/§7): the
                    // authoritative run→agent `agent_call` line carrying the child's `child_thread_id`
                    // + absolute `rollout_path` + metered `tokens_spent`, plus the ordinal/key/opts.
                    // A completed call records `status:completed` with its return; a dead/aborted child
                    // records `status:null`/`return:null` (still carrying whatever linkage the child
                    // produced). Awaited so the line is durable before the promise resolves.
                    let child_thread_id = spawn_outcome.child_thread_id.map(|id| id.to_string());
                    let rollout_path = spawn_outcome
                        .rollout_path
                        .as_ref()
                        .map(|path| path.display().to_string());
                    match &outcome {
                        AgentSpawnOutcome::Completed(value) => {
                            journal
                                .record(
                                    Some(JournalAgentStatus::Completed),
                                    value.clone(),
                                    child_thread_id,
                                    rollout_path,
                                    // A `completed` line MUST carry `tokens_spent` (§7 validate);
                                    // default to 0 if the child reported no usage.
                                    Some(spawn_outcome.tokens_spent.unwrap_or(0)),
                                )
                                .await;
                        }
                        _ => {
                            // Death-is-null: a dead/aborted child is `status:null` + `return:null`.
                            journal
                                .record(
                                    None,
                                    JsonValue::Null,
                                    child_thread_id,
                                    rollout_path,
                                    spawn_outcome.tokens_spent,
                                )
                                .await;
                        }
                    }
                    // Release the reservation on finalize (success AND failure alike). The child's
                    // real usage is already recorded via `record_usage`, so dropping the estimate
                    // reconciles the reservation against actual spend.
                    if reserved {
                        budget.release_reservation(WORKFLOW_AGENT_TURN_TOKEN_ESTIMATE);
                    }
                    // Emit the post-finalize budget state so the crossing turn — even the final
                    // over-ceiling child that ends the run — surfaces `BudgetLimited` (finding #9),
                    // not only when the NEXT `agent()` is attempted.
                    emit_budget_thread_goal(session, &turn_sub_id, budget).await;
                    SpawnAttempt::Finalized(outcome)
                }
            })
            .await;

        match admit_result {
            Ok(outcome) => outcome,
            // Lifetime cap reached (spec §5): terminal and monotonic — surfaced as a JS throw.
            Err(AgentCapReached { .. }) => {
                journal.record_error().await;
                AgentSpawnOutcome::Rejected("AgentCapReached".to_string())
            }
        }
    }

    /// Route a workflow `workflow(nameOrRef, args)` call to the registry-load + nested re-enter
    /// handler ([`run_workflow_by_name`]), resolving to an [`AgentSpawnOutcome`] the isolate settles
    /// the `workflow()` promise with. `cell_id` is the PARENT cell that made the call, used to
    /// recover the parent run id for the nested run's `parent_run_id`.
    async fn spawn_workflow(
        &self,
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
    ) -> AgentSpawnOutcome {
        run_workflow_by_name(&self.exec, &cell_id, &name, args).await
    }

    async fn notify(&self, call_id: String, cell_id: CellId, text: String) -> Result<(), String> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.exec
            .session
            .inject_if_running(vec![ResponseItem::CustomToolCallOutput {
                id: None,
                call_id,
                name: Some(PUBLIC_TOOL_NAME.to_string()),
                output: FunctionCallOutputPayload::from_text(text),
                internal_chat_message_metadata_passthrough: None,
            }])
            .await
            .map_err(|_| {
                format!("failed to inject exec notify message for cell {cell_id}: no active turn")
            })
    }
}

/// Turn a workflow `agent()` child's raw final message into the JS value the isolate promise
/// resolves to (spec §6 structured output).
///
/// - `final_text == None` (a dead/aborted child, or a config-build/spawn/submit failure) → `None`
///   (JS `null`).
/// - `schema == None` (a schemaless call) → `Some(JsonValue::String(final_text))` (a plain JS
///   string), so an ordinary `agent()` still returns the assistant text.
/// - `schema == Some` (structured output) → `serde_json`-parse `final_text`, then, as
///   **defense-in-depth**, re-validate the parsed instance against the JSON Schema with the
///   `jsonschema` crate. Strict mode is engine-enforced for OpenAI providers but not guaranteed for
///   all, so this recheck always runs regardless of what the engine claims. A parse failure, an
///   uncompilable schema, or a non-conformant instance each resolve to `None` (JS `null`) per the
///   death-is-null contract — `agent()` never throws for agent failure.
fn finalize_agent_output(
    final_text: Option<String>,
    schema: Option<&JsonValue>,
) -> Option<JsonValue> {
    let final_text = final_text?;
    let Some(schema) = schema else {
        return Some(JsonValue::String(final_text));
    };
    let parsed: JsonValue = serde_json::from_str(&final_text)
        .map_err(|err| warn!("workflow agent() structured output is not valid JSON: {err}"))
        .ok()?;
    // Belt-and-suspenders: compile the schema and re-validate the parsed instance even though the
    // child was asked for strict mode. An uncompilable schema is treated as a validation failure
    // (null) rather than trusting unvalidated model output.
    let validator = jsonschema::validator_for(schema)
        .map_err(|err| warn!("workflow agent() opts.schema is not a valid JSON Schema: {err}"))
        .ok()?;
    if validator.is_valid(&parsed) {
        Some(parsed)
    } else {
        warn!("workflow agent() structured output failed JSON Schema validation");
        None
    }
}

/// Objective carried on the workflow budget-reporting [`ThreadGoal`]. The ThreadGoal channel requires
/// a non-empty objective; a workflow run reports budget *state* (not a user-authored goal), so a
/// fixed, non-empty label is used.
const WORKFLOW_BUDGET_GOAL_OBJECTIVE: &str = "workflow budget";

/// Emit the workflow budget-reporting [`ThreadGoal`] on the EXISTING ThreadGoal channel (spec §8),
/// carrying the current turn id so the update is scoped to the workflow turn (not a `turn_id: None`
/// session-global overwrite). Emits nothing for an unmetered run.
///
/// NOTE (finding #9, partial): this reuses the user-facing ThreadGoal channel as the §8 reporting
/// surface, so a budget update still visually replaces the client's rendered goal for the thread.
/// Fully scoping/restoring the user's authored goal needs session goal-state plumbing outside this
/// module; the in-scope hardening here is (a) reporting the CONFIGURED ceiling as `token_budget`
/// (not `spent + remaining`), (b) tagging the real `turn_id`, and (c) emitting on the spend crossing
/// so a final over-ceiling child still surfaces `BudgetLimited`.
async fn emit_budget_thread_goal(
    session: &crate::session::session::Session,
    turn_sub_id: &str,
    budget: &RolloutBudget,
) {
    let Some(goal) = build_budget_thread_goal(session.thread_id(), budget) else {
        return;
    };
    let event = Event {
        id: turn_sub_id.to_string(),
        msg: EventMsg::ThreadGoalUpdated(ThreadGoalUpdatedEvent {
            thread_id: goal.thread_id,
            turn_id: Some(turn_sub_id.to_string()),
            goal,
        }),
    };
    session.send_event_raw(event).await;
}

/// Build the budget-reporting [`ThreadGoal`] for a workflow run from the shared, tree-wide
/// [`RolloutBudget`], reusing the EXISTING ThreadGoal channel (spec §8 reporting) so no new protocol
/// types are needed. Returns `None` for an UNMETERED run (`limit()` is `None`), where there is no
/// ceiling to report.
///
/// For a metered run: `token_budget` is the CONFIGURED ceiling (`budget.limit()`), NOT
/// `spent + remaining` — so an overshot 1000-token ceiling with 1500 spent still reports the 1000
/// limit rather than 1500 (finding #9). `tokens_used` is `budget.spent()`, and the status is
/// [`ThreadGoalStatus::BudgetLimited`] once the ceiling is reached (`remaining <= 0`), else
/// [`ThreadGoalStatus::Active`]. Pure and deterministic — the timestamp fields are zeroed so nothing
/// on the workflow path reads a wall clock (no `Date`).
fn build_budget_thread_goal(thread_id: ThreadId, budget: &RolloutBudget) -> Option<ThreadGoal> {
    // `limit()` is the authoritative metered signal: `None` unconfigured (no ceiling to report),
    // `Some(_)` (including `Some(0)`) a real ceiling.
    let limit = budget.limit()?;
    let spent = budget.spent();
    let remaining = budget.remaining();
    let status = if remaining <= 0 {
        ThreadGoalStatus::BudgetLimited
    } else {
        ThreadGoalStatus::Active
    };
    Some(ThreadGoal {
        thread_id,
        objective: WORKFLOW_BUDGET_GOAL_OBJECTIVE.to_string(),
        status,
        token_budget: Some(limit),
        tokens_used: spent,
        time_used_seconds: 0,
        created_at: 0,
        updated_at: 0,
    })
}

/// Enforce the [`WORKFLOW_PROMPT_MAX_BYTES`] ceiling on an `agent()` prompt before it becomes child
/// context. Returns the prompt unchanged when within budget; otherwise truncates it on a UTF-8 char
/// boundary and appends a marker so the child (and any log) sees that truncation happened. Pure and
/// deterministic — no `Date`/`Math`/`rand`.
fn cap_prompt_bytes(prompt: String) -> String {
    if prompt.len() <= WORKFLOW_PROMPT_MAX_BYTES {
        return prompt;
    }
    // Largest char boundary at or below the cap, so we never split a multi-byte code point.
    let mut end = WORKFLOW_PROMPT_MAX_BYTES;
    while end > 0 && !prompt.is_char_boundary(end) {
        end -= 1;
    }
    let mut truncated = prompt[..end].to_string();
    truncated.push_str("\n[workflow agent() prompt truncated: exceeded 64 KiB]");
    warn!(
        "workflow agent() prompt exceeded {WORKFLOW_PROMPT_MAX_BYTES} bytes; truncated to fit the \
         child context budget"
    );
    truncated
}

/// Bound an `agent()` `opts.schema` BEFORE it is threaded into a child prompt or recompiled on
/// return: reject a schema whose serialized size exceeds [`WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES`] or
/// whose nesting exceeds [`WORKFLOW_SCHEMA_MAX_DEPTH`]. The size check runs first so the (cheap)
/// depth walk only ever runs on an already-small structure. Returns an actionable reason on refusal.
fn ensure_schema_within_bounds(schema: &JsonValue) -> Result<(), String> {
    let serialized_len = serde_json::to_vec(schema)
        .map_err(|err| format!("opts.schema could not be serialized: {err}"))?
        .len();
    if serialized_len > WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES {
        return Err(format!(
            "opts.schema is too large ({serialized_len} bytes > {WORKFLOW_SCHEMA_MAX_SERIALIZED_BYTES} byte cap)"
        ));
    }
    let depth = json_depth(schema, WORKFLOW_SCHEMA_MAX_DEPTH);
    if depth > WORKFLOW_SCHEMA_MAX_DEPTH {
        return Err(format!(
            "opts.schema nesting is too deep (exceeds the {WORKFLOW_SCHEMA_MAX_DEPTH}-level cap)"
        ));
    }
    Ok(())
}

/// Iterative (stack-safe) maximum nesting depth of a JSON value, short-circuiting once `limit` is
/// exceeded so an adversarial structure cannot force an unbounded walk. A scalar is depth 1.
fn json_depth(value: &JsonValue, limit: usize) -> usize {
    let mut max_depth = 0usize;
    let mut stack = vec![(value, 1usize)];
    while let Some((node, depth)) = stack.pop() {
        max_depth = max_depth.max(depth);
        if depth > limit {
            // Already over the cap; no need to descend further.
            return max_depth;
        }
        match node {
            JsonValue::Array(items) => {
                for item in items {
                    stack.push((item, depth + 1));
                }
            }
            JsonValue::Object(map) => {
                for item in map.values() {
                    stack.push((item, depth + 1));
                }
            }
            _ => {}
        }
    }
    max_depth
}

#[cfg(test)]
mod prompt_and_schema_bound_tests {
    use super::*;
    use serde_json::json;

    /// A prompt within the byte ceiling is returned verbatim (no marker).
    #[test]
    fn prompt_within_cap_is_unchanged() {
        let prompt = "a".repeat(1024);
        assert_eq!(cap_prompt_bytes(prompt.clone()), prompt);
    }

    /// A prompt over the ceiling is truncated to fit and carries the truncation marker.
    #[test]
    fn oversized_prompt_is_truncated_with_marker() {
        let prompt = "a".repeat(WORKFLOW_PROMPT_MAX_BYTES + 4096);
        let capped = cap_prompt_bytes(prompt);
        assert!(
            capped.len() <= WORKFLOW_PROMPT_MAX_BYTES + 64,
            "truncated prompt (plus marker) must be bounded, got {} bytes",
            capped.len()
        );
        assert!(
            capped.ends_with("truncated: exceeded 64 KiB]"),
            "a truncated prompt must carry the marker"
        );
    }

    /// Truncation never splits a multi-byte UTF-8 code point (the result is always valid UTF-8).
    #[test]
    fn oversized_multibyte_prompt_truncates_on_char_boundary() {
        // 'é' is 2 bytes; a run of them can only be split on an even boundary.
        let prompt = "é".repeat(WORKFLOW_PROMPT_MAX_BYTES);
        let capped = cap_prompt_bytes(prompt);
        // A round-trip through `str` would panic on a bad boundary; reaching here proves validity.
        assert!(capped.contains("truncated"));
    }

    /// A small, shallow schema passes the bounds check.
    #[test]
    fn small_schema_is_within_bounds() {
        let schema = json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
        });
        assert!(ensure_schema_within_bounds(&schema).is_ok());
    }

    /// A schema whose serialized size blows the byte cap is rejected with a size reason.
    #[test]
    fn oversized_schema_is_rejected() {
        // A wide object with many string properties easily exceeds the 32 KiB serialized cap.
        let mut props = serde_json::Map::new();
        for i in 0..4000 {
            props.insert(format!("field_{i}"), json!({ "type": "string" }));
        }
        let schema = json!({ "type": "object", "properties": props });
        let err = ensure_schema_within_bounds(&schema).expect_err("oversized schema must reject");
        assert!(
            err.contains("too large"),
            "reason must name the size: {err}"
        );
    }

    /// A schema nested past the depth cap is rejected with a depth reason.
    #[test]
    fn overdeep_schema_is_rejected() {
        // Build `{"a":{"a":{...}}}` nested well past the depth cap.
        let mut node = json!({ "type": "string" });
        for _ in 0..(WORKFLOW_SCHEMA_MAX_DEPTH + 5) {
            node = json!({ "type": "object", "properties": { "a": node } });
        }
        let err = ensure_schema_within_bounds(&node).expect_err("overdeep schema must reject");
        assert!(
            err.contains("too deep"),
            "reason must name the depth: {err}"
        );
    }

    /// `json_depth` counts nesting levels and short-circuits at the limit.
    #[test]
    fn json_depth_counts_levels() {
        assert_eq!(json_depth(&json!(1), 64), 1);
        assert_eq!(json_depth(&json!({ "a": 1 }), 64), 2);
        assert_eq!(json_depth(&json!({ "a": { "b": 1 } }), 64), 3);
        assert_eq!(json_depth(&json!([[[1]]]), 64), 4);
    }
}

#[cfg(test)]
mod finalize_agent_output_tests {
    use super::*;
    use serde_json::json;

    /// The structured-output schema used across these cases: an object requiring a string `answer`.
    fn answer_schema() -> JsonValue {
        json!({
            "type": "object",
            "properties": { "answer": { "type": "string" } },
            "required": ["answer"],
            "additionalProperties": false,
        })
    }

    /// A schema fixture whose final text is conformant JSON returns the parsed object (not a string).
    #[test]
    fn schema_fixture_returns_validated_object() {
        let schema = answer_schema();
        let out = finalize_agent_output(Some(r#"{"answer":"42"}"#.to_string()), Some(&schema));
        assert_eq!(out, Some(json!({ "answer": "42" })));
        // Specifically an object, never the raw string.
        assert!(matches!(out, Some(JsonValue::Object(_))));
    }

    /// Well-formed JSON that violates the schema (wrong type) is rejected by the recheck → null.
    #[test]
    fn nonconformant_json_resolves_to_null() {
        let schema = answer_schema();
        // `answer` must be a string; a number is well-formed JSON but non-conformant.
        let out = finalize_agent_output(Some(r#"{"answer":42}"#.to_string()), Some(&schema));
        assert_eq!(out, None);
    }

    /// A conformant-looking object with an extra key is rejected under `additionalProperties:false`.
    /// This is the belt-and-suspenders case: even if a non-strict engine let this through, our
    /// recheck still runs and rejects it.
    #[test]
    fn extra_property_rejected_by_recheck_even_if_engine_claims_strict() {
        let schema = answer_schema();
        let out = finalize_agent_output(
            Some(r#"{"answer":"ok","leaked":true}"#.to_string()),
            Some(&schema),
        );
        assert_eq!(out, None);
    }

    /// Malformed (non-JSON) final text under a schema resolves to null rather than throwing.
    #[test]
    fn malformed_json_resolves_to_null() {
        let schema = answer_schema();
        let out = finalize_agent_output(Some("not json at all".to_string()), Some(&schema));
        assert_eq!(out, None);
    }

    /// An uncompilable JSON Schema is treated as a validation failure (null), never a trusted pass.
    #[test]
    fn uncompilable_schema_resolves_to_null() {
        // `type` must be a string/array of strings; a number makes the schema invalid.
        let schema = json!({ "type": 123 });
        let out = finalize_agent_output(Some(r#"{"answer":"ok"}"#.to_string()), Some(&schema));
        assert_eq!(out, None);
    }

    /// Without a schema, `agent()` returns the plain final assistant text as a JSON string.
    #[test]
    fn without_schema_returns_plain_text_string() {
        let out = finalize_agent_output(Some("plain final text".to_string()), None);
        assert_eq!(out, Some(JsonValue::String("plain final text".to_string())));
    }

    /// Without a schema, text that merely *looks* like JSON is still returned verbatim as a string —
    /// no parsing happens on the schemaless path.
    #[test]
    fn without_schema_does_not_parse_jsonish_text() {
        let out = finalize_agent_output(Some(r#"{"a":1}"#.to_string()), None);
        assert_eq!(out, Some(JsonValue::String(r#"{"a":1}"#.to_string())));
    }

    /// A dead/aborted child (`None` final text) resolves to null with or without a schema.
    #[test]
    fn dead_agent_resolves_to_null() {
        let schema = answer_schema();
        assert_eq!(finalize_agent_output(None, Some(&schema)), None);
        assert_eq!(finalize_agent_output(None, None), None);
    }
}

#[cfg(test)]
mod budget_thread_goal_tests {
    use super::*;
    use crate::rollout_budget::RolloutBudget;
    use crate::rollout_budget::workflow_output_weight_config;
    use codex_protocol::protocol::TokenUsage;

    fn output_usage(output_tokens: i64) -> TokenUsage {
        TokenUsage {
            input_tokens: 0,
            cached_input_tokens: 0,
            output_tokens,
            reasoning_output_tokens: 0,
            total_tokens: output_tokens,
        }
    }

    /// An UNMETERED run leaves the budget unconfigured (both getters read 0); there is no ceiling to
    /// report, so no ThreadGoal is produced (nothing is emitted for an unmetered workflow).
    #[test]
    fn no_thread_goal_for_unmetered_run() {
        let budget = RolloutBudget::default();
        assert!(build_budget_thread_goal(ThreadId::new(), &budget).is_none());
    }

    /// Below the ceiling a metered run reports `Active` carrying `token_budget = budget.total` and
    /// `tokens_used = budget.spent()` on the existing ThreadGoal channel.
    #[test]
    fn thread_goal_reports_active_below_ceiling() {
        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(1_000));
        budget.record_usage(&output_usage(100));

        let thread_id = ThreadId::new();
        let goal =
            build_budget_thread_goal(thread_id, &budget).expect("metered run reports a goal");
        assert_eq!(goal.thread_id, thread_id);
        assert_eq!(goal.status, ThreadGoalStatus::Active);
        assert_eq!(goal.token_budget, Some(1_000));
        assert_eq!(goal.tokens_used, 100);
    }

    /// At the ceiling (`remaining <= 0`) the run reports [`ThreadGoalStatus::BudgetLimited`] — the
    /// budget-limited condition surfaced through the existing ThreadGoal channel (no new protocol
    /// types), with `tokens_used` at the exhausted total.
    #[test]
    fn thread_goal_reports_budget_limited_at_ceiling() {
        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(1_000));
        budget.record_usage(&output_usage(1_000));

        let goal =
            build_budget_thread_goal(ThreadId::new(), &budget).expect("metered run reports a goal");
        assert_eq!(goal.status, ThreadGoalStatus::BudgetLimited);
        assert_eq!(goal.token_budget, Some(1_000));
        assert_eq!(goal.tokens_used, 1_000);
    }

    /// Overshoot (an in-flight turn pushing spend past the limit) still reports `BudgetLimited`,
    /// and `token_budget` reports the CONFIGURED ceiling (1000), NOT the overshot spend (1500) —
    /// finding #9: an overshot 1000-limit with 1500 spent must report 1000, not 1500.
    #[test]
    fn thread_goal_budget_limited_on_overshoot() {
        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(1_000));
        budget.record_usage(&output_usage(1_500));

        let goal =
            build_budget_thread_goal(ThreadId::new(), &budget).expect("metered run reports a goal");
        assert_eq!(goal.status, ThreadGoalStatus::BudgetLimited);
        assert_eq!(goal.tokens_used, 1_500);
        // token_budget is the configured ceiling, not spent+remaining.
        assert_eq!(goal.token_budget, Some(1_000));
    }

    /// An explicit zero ceiling is metered: it reports a goal with `token_budget: Some(0)` and
    /// `BudgetLimited` immediately (there is no headroom).
    #[test]
    fn thread_goal_reports_zero_ceiling_as_budget_limited() {
        let budget = RolloutBudget::default();
        budget.configure(workflow_output_weight_config(0));
        let goal =
            build_budget_thread_goal(ThreadId::new(), &budget).expect("zero ceiling is metered");
        assert_eq!(goal.status, ThreadGoalStatus::BudgetLimited);
        assert_eq!(goal.token_budget, Some(0));
        assert_eq!(goal.tokens_used, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A workflow `agent()` call issues exactly one `DispatchMessage::SpawnAgent` (carrying the
    /// call's prompt/ordinal/opts) into the dispatch channel — one dispatch per `AgentCall`, never
    /// more.
    #[tokio::test]
    async fn spawn_agent_enqueues_one_spawn_agent_dispatch() {
        let broker = Arc::new(CodeModeDispatchBroker::new(Arc::new(
            WorkflowRunLedger::default(),
        )));

        // `spawn_agent` blocks awaiting a response the (absent) worker never sends, so drive it on a
        // task and inspect the message it enqueued on the shared dispatch channel.
        let call = {
            let broker = Arc::clone(&broker);
            tokio::spawn(async move {
                broker
                    .spawn_agent(
                        CellId::new("cell-1".to_string()),
                        "spawn me".to_string(),
                        7,
                        AgentCallOpts::default(),
                        CancellationToken::new(),
                    )
                    .await
            })
        };

        let message = tokio::time::timeout(Duration::from_secs(1), broker.dispatch_rx.recv())
            .await
            .expect("a dispatch message was enqueued")
            .expect("dispatch channel open");
        match message {
            DispatchMessage::SpawnAgent {
                cell_id,
                prompt,
                ordinal,
                ..
            } => {
                assert_eq!(cell_id, CellId::new("cell-1".to_string()));
                assert_eq!(prompt, "spawn me");
                assert_eq!(ordinal, 7);
            }
            _ => panic!("expected DispatchMessage::SpawnAgent"),
        }

        // Exactly one dispatch per `agent()` call: nothing else was enqueued.
        assert!(
            broker.dispatch_rx.try_recv().is_err(),
            "spawn_agent must enqueue exactly one dispatch message"
        );

        call.abort();
    }

    /// A cancelled `agent()` call resolves to `Failed` (JS null) without ever throwing.
    #[tokio::test]
    async fn spawn_agent_resolves_to_failed_when_cancelled_before_dispatch() {
        let broker = CodeModeDispatchBroker::new(Arc::new(WorkflowRunLedger::default()));
        let cancellation_token = CancellationToken::new();
        cancellation_token.cancel();

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            broker.spawn_agent(
                CellId::new("cell-1".to_string()),
                "prompt".to_string(),
                0,
                AgentCallOpts::default(),
                cancellation_token,
            ),
        )
        .await
        .expect("spawn_agent resolved promptly");
        assert!(
            matches!(result, AgentSpawnOutcome::Failed),
            "a cancelled call must resolve to Failed (null), never throw"
        );
    }
}

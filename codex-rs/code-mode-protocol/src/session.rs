use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::CodeModeNestedToolCall;
use crate::ExecuteRequest;
use crate::RuntimeResponse;
use crate::WaitOutcome;
use crate::WaitRequest;
use crate::WorkflowBudgetSnapshot;
use codex_protocol::protocol::WorkflowEvent;

pub type CodeModeSessionResultFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub type CodeModeSessionProviderFuture<'a> =
    CodeModeSessionResultFuture<'a, Arc<dyn CodeModeSession>>;
pub type ToolInvocationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JsonValue, String>> + Send + 'a>>;
pub type NotificationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
pub type WorkflowBudgetSnapshotFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<WorkflowBudgetSnapshot>, String>> + Send + 'a>>;

/// Runtime-to-core workflow progress carried by the negotiated `workflow-v1` host envelope.
///
/// Renderer-neutral events are authored in the isolate where source order is known. Terminal
/// completion is intentionally a separate signal: core owns the live budget and translates it to
/// the public `WorkflowRunEndEvent` at the session boundary.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkflowHostProgress {
    Event { event: Box<WorkflowEvent> },
    Complete { status: WorkflowHostCompletion },
}

/// Terminal runtime outcome before core adds run budget counters.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowHostCompletion {
    Completed,
    Errored(String),
    Interrupted,
}
/// The three-way resolution of a workflow `agent(prompt, opts?)` spawn, produced host-side and
/// carried back to the isolate so the `agent()` promise can RESOLVE, resolve-to-null, or REJECT.
///
/// This is the shared seam type between core's spawn path and the code-mode bridge. `agent()` never
/// throws for *agent* failure (death-is-null), but an *admission-time* cap/budget rejection is a
/// distinct outcome the isolate surfaces as a thrown promise rejection.
#[derive(Clone, Debug)]
pub enum AgentSpawnOutcome {
    /// Resolve the `agent()` promise with this value: a plain JS string (a schemaless call, carried
    /// as [`JsonValue::String`]) or the validated JSON object for an `opts.schema` call (`§6`).
    Completed(JsonValue),
    /// Resolve the `agent()` promise to JS `null` — death-is-null. Covers a dead/aborted agent, an
    /// unresolvable model/role/effort, an `opts.schema` parse/validation failure, or a host that
    /// does not support workflow spawning.
    Failed,
    /// REJECT (throw) the `agent()` promise with this message. Reserved for an admission-time
    /// scheduler lifetime-cap / budget rejection (e.g. `"AgentCapReached"` / `"BudgetExceeded"`),
    /// which is *not* an agent failure and must not be silently swallowed as `null`.
    Rejected(String),
}

/// Future returned by [`CodeModeSessionDelegate::spawn_agent`]. Resolves to an [`AgentSpawnOutcome`]
/// that tells the isolate how to settle the `agent()` promise (resolve with a value, resolve to
/// `null`, or throw).
pub type AgentSpawnFuture<'a> = Pin<Box<dyn Future<Output = AgentSpawnOutcome> + Send + 'a>>;

/// Live, thread-safe view of one workflow run's runtime-owned budget mirror,
/// backing the native `budget.spent()` / `budget.remaining()` globals.
///
/// The mirror starts from [`ExecuteRequest::workflow_budget`] and is refreshed
/// through [`CodeModeSessionDelegate::workflow_budget_snapshot`] after callbacks
/// that can change spend. This keeps the isolate API identical for in-process and
/// process-owned hosts without exposing core's session-wide rollout budget.
pub trait WorkflowBudgetHandle: Send + Sync {
    /// Configured `budget.total` ceiling (pure output-token spend, §8).
    fn total(&self) -> i64;
    /// Live output-token spend charged to this workflow run.
    fn spent(&self) -> i64;
    /// Live effective remaining budget, clamped at 0.
    fn remaining(&self) -> i64;
}

#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct CellId(String);

impl CellId {
    pub fn new(value: String) -> Self {
        Self(value)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for CellId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

pub struct StartedCell {
    pub cell_id: CellId,
    initial_response: CodeModeSessionResultFuture<'static, RuntimeResponse>,
}

impl StartedCell {
    pub fn new(cell_id: CellId, initial_response_rx: oneshot::Receiver<RuntimeResponse>) -> Self {
        Self {
            cell_id,
            initial_response: Box::pin(async move {
                initial_response_rx
                    .await
                    .map_err(|_| "exec runtime ended unexpectedly".to_string())
            }),
        }
    }

    pub fn from_result_receiver(
        cell_id: CellId,
        initial_response_rx: oneshot::Receiver<Result<RuntimeResponse, String>>,
    ) -> Self {
        Self {
            cell_id,
            initial_response: Box::pin(async move {
                initial_response_rx
                    .await
                    .map_err(|_| "exec runtime ended unexpectedly".to_string())?
            }),
        }
    }

    pub async fn initial_response(self) -> Result<RuntimeResponse, String> {
        self.initial_response.await
    }
}

/// Host callbacks used by a code-mode session while cells are executing.
pub trait CodeModeSessionDelegate: Send + Sync {
    fn invoke_tool<'a>(
        &'a self,
        invocation: CodeModeNestedToolCall,
        cancellation_token: CancellationToken,
    ) -> ToolInvocationFuture<'a>;

    fn notify<'a>(
        &'a self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> NotificationFuture<'a>;

    /// Spawn a workflow subagent for an `agent(prompt, opts?)` call and resolve to the child's final
    /// answer.
    ///
    /// `ordinal` is the deterministic source-order invocation ordinal stamped by the runtime
    /// (`RuntimeState.next_agent_ordinal`), used host-side to derive a replay-stable subagent
    /// nickname. The returned future resolves to an [`AgentSpawnOutcome`]: `Completed(value)` on
    /// success (a JSON string for a schemaless call, or the validated JSON object for an
    /// `opts.schema` call), `Failed` (JS `null`) for any agent failure (a dead/aborted agent, an
    /// unresolvable model/role/effort, a `schema` parse/validation failure, or a host that does not
    /// support workflow spawning) — `agent()` never throws for agent failure — or `Rejected(msg)` to
    /// throw an admission-time cap/budget rejection in the isolate. The default implementation
    /// resolves to `Failed` so non-workflow hosts need no changes.
    fn spawn_agent<'a>(
        &'a self,
        cell_id: CellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        prompt: String,
        ordinal: u64,
        opts: crate::AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        let _ = (
            cell_id,
            node_id,
            parent_node_id,
            phase,
            prompt,
            ordinal,
            opts,
            cancellation_token,
        );
        Box::pin(async { AgentSpawnOutcome::Failed })
    }

    /// Run a saved workflow inline for a `workflow(nameOrRef, args)` call and resolve to the nested
    /// run's top-level result.
    ///
    /// `name` is the caller-supplied `nameOrRef`; the host resolves it against the core-workflows
    /// registry, loads the named script, and re-enters the runtime one level deep with `args`
    /// injected as the nested run's read-only `args` global. The returned future resolves to an
    /// [`AgentSpawnOutcome`]: `Completed(value)` with the nested run's top-level result,
    /// `Failed` (JS `null`) when the nested run produces no result, or `Rejected(msg)` to throw in
    /// the isolate (e.g. a name that does not resolve in the registry, or a nested script error).
    /// The default implementation resolves to `Failed` so non-workflow hosts need no changes.
    fn spawn_workflow<'a>(
        &'a self,
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        let _ = (cell_id, name, args, cancellation_token);
        Box::pin(async { AgentSpawnOutcome::Failed })
    }

    /// Refresh the run-local budget mirror after an `agent()`, replay, or nested
    /// workflow callback. Process-owned hosts route this over IPC; in-process
    /// hosts use the identical callback so both observe the same ordering.
    fn workflow_budget_snapshot<'a>(&'a self, cell_id: CellId) -> WorkflowBudgetSnapshotFuture<'a> {
        let _ = cell_id;
        Box::pin(async { Ok(None) })
    }

    /// Journal a workflow `phase(title)` marker for the run executing in `cell_id` (§7 `phase`
    /// line). An error stops the workflow with a generic public failure while the host detail stays
    /// diagnostic-only. The default is a successful no-op so non-workflow hosts need no changes.
    fn journal_phase<'a>(&'a self, cell_id: CellId, title: String) -> NotificationFuture<'a> {
        let _ = (cell_id, title);
        Box::pin(async { Ok(()) })
    }

    /// Journal a workflow `log(message)` marker for the run executing in `cell_id` (§7 `log` line).
    /// An error stops the workflow with a generic public failure while the host detail stays
    /// diagnostic-only. The default is a successful no-op so non-workflow hosts need no changes.
    fn journal_log<'a>(&'a self, cell_id: CellId, message: String) -> NotificationFuture<'a> {
        let _ = (cell_id, message);
        Box::pin(async { Ok(()) })
    }

    /// Handle a prefix-replay cache hit for the run executing in `cell_id` (§7 "Resume algorithm"
    /// step 3): re-append the replayed `agent_call` line to the run's journal and charge its
    /// `tokens_spent` to that run's local meter, WITHOUT spawning a subagent. Session accounting is
    /// unchanged because replay performs no model turn.
    ///
    /// `entry` is a raw JSON `agent_call` record (a serialized
    /// `codex_workflow_journal::AgentCallLine`), mirroring [`replay_entries`] so this protocol trait
    /// stays free of a dependency on the journal crate. The default is a no-op so non-workflow hosts
    /// — and every host that neither journals nor meters — need no changes. Any error is fatal to
    /// the workflow: callers must not continue from a replay prefix that was not durably recorded
    /// and charged.
    ///
    /// [`replay_entries`]: ExecuteRequest::replay_entries
    fn replay_agent<'a>(
        &'a self,
        cell_id: CellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        entry: JsonValue,
    ) -> NotificationFuture<'a> {
        let _ = (cell_id, node_id, parent_node_id, phase, entry);
        Box::pin(async { Ok(()) })
    }

    /// Deliver source-ordered workflow progress for `cell_id` to core. The default is a no-op so
    /// plain code-mode hosts and tests that do not negotiate `workflow-v1` remain unchanged.
    fn workflow_progress<'a>(
        &'a self,
        cell_id: CellId,
        progress: WorkflowHostProgress,
    ) -> NotificationFuture<'a> {
        let _ = (cell_id, progress);
        Box::pin(async { Ok(()) })
    }

    /// Releases delegate state associated with a cell after it reaches a terminal state.
    fn cell_closed(&self, cell_id: &CellId);
}

/// A durable code-mode session owned by one Codex thread.
///
/// Cells executed in the same session share stored values. Separate sessions
/// must keep those values isolated. Implementations may execute cells
/// in-process or remotely.
pub trait CodeModeSession: Send + Sync {
    fn execute<'a>(
        &'a self,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'a, StartedCell>;

    fn wait<'a>(&'a self, request: WaitRequest) -> CodeModeSessionResultFuture<'a, WaitOutcome>;

    fn terminate<'a>(&'a self, cell_id: CellId) -> CodeModeSessionResultFuture<'a, WaitOutcome>;

    fn shutdown<'a>(&'a self) -> CodeModeSessionResultFuture<'a, ()>;
}

/// Creates code-mode sessions for Codex threads.
///
/// Implementations may share a remote host process across all sessions created
/// by one provider.
pub trait CodeModeSessionProvider: Send + Sync {
    fn create_session<'a>(
        &'a self,
        delegate: Arc<dyn CodeModeSessionDelegate>,
    ) -> CodeModeSessionProviderFuture<'a>;
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

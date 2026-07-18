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

pub type CodeModeSessionResultFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub type CodeModeSessionProviderFuture<'a> =
    CodeModeSessionResultFuture<'a, Arc<dyn CodeModeSession>>;
pub type ToolInvocationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JsonValue, String>> + Send + 'a>>;
pub type NotificationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
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
        prompt: String,
        ordinal: u64,
        opts: crate::AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> AgentSpawnFuture<'a> {
        let _ = (cell_id, prompt, ordinal, opts, cancellation_token);
        Box::pin(async { AgentSpawnOutcome::Failed })
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

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
use crate::ExecuteOutputPolicy;
use crate::ExecuteRequest;
use crate::RuntimeResponse;
use crate::SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE;
use crate::WaitOutcome;
use crate::WaitRequest;

pub type CodeModeSessionResultFuture<'a, T> =
    Pin<Box<dyn Future<Output = Result<T, String>> + Send + 'a>>;
pub type CodeModeSessionProviderFuture<'a> =
    CodeModeSessionResultFuture<'a, Arc<dyn CodeModeSession>>;
pub type ToolInvocationFuture<'a> =
    Pin<Box<dyn Future<Output = Result<JsonValue, String>> + Send + 'a>>;
pub type NotificationFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;

const STARTED_CELL_BINDING_UNAVAILABLE: &str = "exact started-cell binding is unavailable";
const STARTED_CELL_BINDING_IDENTITY_MISMATCH: &str = "started-cell binding identity mismatch";

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

/// Operations pinned to the session binding and generation that started one cell.
///
/// Implementations must retain the exact transport/session authority used by the
/// corresponding execute request. They must never reconnect or resolve a newer
/// session generation while waiting for or terminating the cell.
pub trait StartedCellBinding: Send + Sync {
    fn cell_id(&self) -> &CellId;

    fn wait<'a>(&'a self, yield_time_ms: u64) -> CodeModeSessionResultFuture<'a, WaitOutcome>;

    fn terminate<'a>(&'a self) -> CodeModeSessionResultFuture<'a, WaitOutcome>;
}

/// A started cell paired with operations bound to its exact session generation.
pub struct BoundStartedCell {
    started_cell: StartedCell,
    binding: Arc<dyn StartedCellBinding>,
}

impl BoundStartedCell {
    pub fn new(
        started_cell: StartedCell,
        binding: Arc<dyn StartedCellBinding>,
    ) -> Result<Self, String> {
        if &started_cell.cell_id != binding.cell_id() {
            return Err(STARTED_CELL_BINDING_IDENTITY_MISMATCH.to_string());
        }
        Ok(Self {
            started_cell,
            binding,
        })
    }

    pub fn cell_id(&self) -> &CellId {
        &self.started_cell.cell_id
    }

    pub fn into_parts(self) -> (StartedCell, Arc<dyn StartedCellBinding>) {
        (self.started_cell, self.binding)
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

    /// Starts a cell and returns operations pinned to the exact starting session generation.
    ///
    /// Implementations that cannot preserve that binding must fail instead of
    /// falling back to the reconnecting [`Self::wait`] and [`Self::terminate`]
    /// methods.
    fn execute_bound(
        self: Arc<Self>,
        request: ExecuteRequest,
    ) -> CodeModeSessionResultFuture<'static, BoundStartedCell> {
        let error = match request.output_policy {
            ExecuteOutputPolicy::Ordinary => STARTED_CELL_BINDING_UNAVAILABLE,
            ExecuteOutputPolicy::SavedWorkflow => SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE,
        };
        Box::pin(async move { Err(error.to_string()) })
    }

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

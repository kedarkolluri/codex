use std::fmt;
use std::future::Future;
use std::time::Duration;

use std::sync::Arc;

use codex_code_mode_protocol::AgentCallOpts;
use codex_code_mode_protocol::AgentSpawnOutcome;
use codex_code_mode_protocol::WorkflowBudgetHandle;
use serde_json::Value as JsonValue;
use tokio_util::sync::CancellationToken;

/// Identifies one execution cell within a session runtime.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CellId(String);

impl CellId {
    pub(crate) fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Selects the next observable frontier for a running cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObserveMode {
    YieldAfter(Duration),
    PendingFrontier,
}

/// An observable cell lifecycle event.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum CellEvent {
    Yielded {
        content_items: Vec<OutputItem>,
    },
    Pending {
        content_items: Vec<OutputItem>,
        pending_tool_call_ids: Vec<String>,
    },
    Completed {
        content_items: Vec<OutputItem>,
        error_text: Option<String>,
    },
    Terminated {
        content_items: Vec<OutputItem>,
    },
}

/// Output emitted by a cell since its preceding observation.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum OutputItem {
    Text {
        text: String,
    },
    Image {
        image_url: String,
        detail: Option<ImageDetail>,
    },
}

/// Requested image fidelity for an output image.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageDetail {
    Auto,
    Low,
    High,
    Original,
}

/// Transport-neutral input for creating a cell.
///
/// The owning session assigns the cell ID when it admits the request.
pub(crate) struct CreateCellRequest {
    pub(crate) tool_call_id: String,
    pub(crate) enabled_tools: Vec<ToolDefinition>,
    pub(crate) source: String,
    /// Explicit workflow invocation mode threaded from the workflow handler
    /// through the protocol `ExecuteRequest`. Gates the workflow-only narrator
    /// globals; see [`codex_code_mode_protocol::ExecuteRequest::workflow`].
    pub(crate) workflow: bool,
    /// Invocation JSON threaded from the workflow handler; installed read-only as
    /// the `args` global for workflow runs. See
    /// [`codex_code_mode_protocol::ExecuteRequest::args`].
    pub(crate) args: Option<JsonValue>,
    /// Host-minted uuid v7 run identifier; exposed read-only as `workflow.runId`.
    /// See [`codex_code_mode_protocol::ExecuteRequest::run_id`].
    pub(crate) run_id: Option<String>,
}

/// Tool metadata exposed to code running inside a cell.
pub(crate) struct ToolDefinition {
    pub(crate) name: String,
    pub(crate) tool_name: ToolName,
    pub(crate) description: String,
    pub(crate) kind: ToolKind,
}

/// A tool name with an optional namespace.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ToolName {
    pub(crate) name: String,
    pub(crate) namespace: Option<String>,
}

/// The JavaScript calling convention for a tool.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ToolKind {
    Function,
    Freeform,
}

/// A nested tool request emitted by a running cell.
pub(crate) struct NestedToolCall {
    pub(crate) cell_id: CellId,
    pub(crate) runtime_tool_call_id: String,
    pub(crate) tool_name: ToolName,
    pub(crate) tool_kind: ToolKind,
    pub(crate) input: Option<JsonValue>,
}

/// Host callbacks used by cells owned by a [`super::SessionRuntime`].
///
/// Implementations must honor cancellation tokens. `cell_closed` is called
/// after the runtime has stopped routing requests to the cell.
pub(crate) trait SessionRuntimeDelegate: Send + Sync + 'static {
    fn invoke_tool(
        &self,
        invocation: NestedToolCall,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<JsonValue, String>> + Send;

    fn notify(
        &self,
        call_id: String,
        cell_id: CellId,
        text: String,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = Result<(), String>> + Send;

    /// Spawn a workflow subagent for an `agent(prompt, opts?)` call, resolving to an
    /// [`AgentSpawnOutcome`]: `Completed` (a JSON string when schemaless, or the validated
    /// `opts.schema` object), `Failed` (JS `null`), or `Rejected` (throw). The default resolves to
    /// `Failed` so delegates that do not support workflow spawning need no changes.
    fn spawn_agent(
        &self,
        cell_id: CellId,
        prompt: String,
        ordinal: u64,
        opts: AgentCallOpts,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = AgentSpawnOutcome> + Send {
        let _ = (cell_id, prompt, ordinal, opts, cancellation_token);
        async { AgentSpawnOutcome::Failed }
    }

    /// Run a saved workflow inline for a `workflow(nameOrRef, args)` call, resolving to an
    /// [`AgentSpawnOutcome`] (`Completed` with the nested run's result, `Failed` -> JS `null`, or
    /// `Rejected` -> throw). The default resolves to `Failed` so delegates without nested-workflow
    /// support need no changes.
    fn spawn_workflow(
        &self,
        cell_id: CellId,
        name: String,
        args: Option<JsonValue>,
        cancellation_token: CancellationToken,
    ) -> impl Future<Output = AgentSpawnOutcome> + Send {
        let _ = (cell_id, name, args, cancellation_token);
        async { AgentSpawnOutcome::Failed }
    }

    /// The live shared token-budget handle backing the workflow `budget` global, or `None` when this
    /// delegate runs no budgeted workflow. The default returns `None` so delegates without a budget
    /// need no changes.
    fn budget_handle(&self) -> Option<Arc<dyn WorkflowBudgetHandle>> {
        None
    }

    fn cell_closed(&self, cell_id: &CellId);
}

/// A failure reported by a session runtime operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Error {
    ShuttingDown,
    CellIdSpaceExhausted,
    DuplicateCell(CellId),
    MissingCell(CellId),
    BusyObserver(CellId),
    AlreadyTerminating(CellId),
    ClosedCell(CellId),
    Runtime(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("code mode session is shutting down"),
            Self::CellIdSpaceExhausted => {
                formatter.write_str("code mode session exhausted its cell ID space")
            }
            Self::DuplicateCell(cell_id) => write!(formatter, "exec cell {cell_id} already exists"),
            Self::MissingCell(cell_id) => write!(formatter, "exec cell {cell_id} not found"),
            Self::BusyObserver(cell_id) => {
                write!(
                    formatter,
                    "exec cell {cell_id} already has an active observer"
                )
            }
            Self::AlreadyTerminating(cell_id) => {
                write!(formatter, "exec cell {cell_id} is already terminating")
            }
            Self::ClosedCell(cell_id) => {
                write!(formatter, "exec cell {cell_id} closed unexpectedly")
            }
            Self::Runtime(error_text) => formatter.write_str(error_text),
        }
    }
}

impl std::error::Error for Error {}

use super::CollabAgentStatus;
use super::TokenUsageBreakdown;
use codex_protocol::openai_models::ReasoningEffort;
use codex_utils_path_uri::LegacyAppPathString;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;
use ts_rs::TS;

/// Notification emitted when watched saved-workflow files change.
///
/// Treat this as an invalidation signal and re-discover saved workflows when
/// refreshed workflow metadata is needed. Mirrors [`super::SkillsChangedNotification`].
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS, Default)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowsChangedNotification {}

/// Request a deterministic page of saved workflows visible to a loaded thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListParams {
    pub thread_id: String,
    /// Opaque pagination cursor returned by a previous call.
    #[ts(optional = nullable)]
    pub cursor: Option<String>,
    /// Optional page size. The server applies a hard upper bound.
    #[ts(optional = nullable)]
    pub limit: Option<u32>,
}

/// Precedence scope that supplied a saved workflow.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowScope {
    Project,
    Personal,
    CodexHome,
}

/// Picker-safe saved-workflow metadata. Workflow source bodies are never exposed.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowMetadata {
    pub name: String,
    pub description: String,
    pub phases: Vec<String>,
    pub scope: WorkflowScope,
    pub path: LegacyAppPathString,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowListResponse {
    pub data: Vec<WorkflowMetadata>,
    /// Opaque cursor to pass to the next call, or `None` after the final page.
    pub next_cursor: Option<String>,
}

/// Read the reconciled durable lifecycle status of one thread-owned workflow run.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct WorkflowReadParams {
    pub thread_id: String,
    pub run_id: String,
}

/// Durable lifecycle status of a workflow run.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowRunStatus {
    Running,
    Completed,
    Stopped,
    Paused,
    Failed,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowReadResponse {
    pub run_id: String,
    pub status: WorkflowRunStatus,
}

/// Start a saved workflow visible to an already-loaded thread.
///
/// The API intentionally accepts only a registry name. Clients cannot submit
/// workflow source or a filesystem path through this request.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct WorkflowStartParams {
    pub thread_id: String,
    pub name: String,
    #[ts(optional = nullable)]
    pub args: Option<JsonValue>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStartResponse {
    pub run_id: String,
}

/// Save the exact durable script of a workflow run owned by a loaded thread.
///
/// The request intentionally accepts neither source text nor a filesystem path.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct WorkflowSaveParams {
    pub thread_id: String,
    pub run_id: String,
    pub name: String,
    pub scope: WorkflowSaveScope,
    /// Replacing an existing saved workflow must be requested explicitly.
    pub overwrite: bool,
}

/// Destination scope accepted by [`WorkflowSaveParams`].
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowSaveScope {
    /// Save under the owning thread's local `<cwd>/.codex/workflows` root.
    Project,
    /// Save under the app-server host's `$HOME/.agents/workflows` root.
    Personal,
}

/// Result of a script-only workflow save.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowSaveDisposition {
    Created,
    Overwritten,
    Conflict,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowSaveResponse {
    pub disposition: WorkflowSaveDisposition,
}

/// Stop an active workflow run owned by an already-loaded thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct WorkflowStopParams {
    pub thread_id: String,
    pub run_id: String,
}

/// Whether this request initiated cancellation or joined an existing request.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowStopDisposition {
    Applied,
    AlreadyRequested,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStopResponse {
    pub disposition: WorkflowStopDisposition,
}

/// Pause an active workflow run owned by an already-loaded thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct WorkflowPauseParams {
    pub thread_id: String,
    pub run_id: String,
}

/// Whether this request initiated checkpoint publication or joined an existing request.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowPauseDisposition {
    Applied,
    AlreadyRequested,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowPauseResponse {
    pub disposition: WorkflowPauseDisposition,
}

/// Resume a paused workflow from its immutable durable checkpoint.
///
/// The request intentionally accepts only the source run identity. Workflow
/// source, name, arguments, and filesystem paths remain server-private.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct WorkflowResumeParams {
    pub thread_id: String,
    pub run_id: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowResumeResponse {
    /// Fresh successor run, shared by duplicate resume requests.
    pub run_id: String,
}

/// User action for one exact live workflow-agent attempt.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowAgentControlAction {
    Skip,
    Retry,
}

/// Control one exact workflow-agent attempt owned by an already-loaded thread.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentControlParams {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub node_id: u64,
    #[ts(type = "number")]
    pub attempt: u32,
    pub action: WorkflowAgentControlAction,
}

/// Result of controlling one exact workflow-agent attempt.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "disposition", rename_all = "camelCase")]
#[ts(tag = "disposition", rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowAgentControlResponse {
    Skipped,
    RetryScheduled {
        /// Fresh zero-based generation scheduled for the same logical node.
        #[ts(type = "number")]
        attempt: u32,
    },
    RetryLimitReached,
}

/// Runtime state of a workflow phase after a phase-boundary event.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowPhaseStatus {
    Active,
    Completed,
}

/// Execution semantics for a workflow group.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowGroupKind {
    Parallel,
    Pipeline,
}

/// Exact reason a workflow run reached its terminal state.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowRunTerminalReason {
    Completed,
    Failed,
    Interrupted,
    Stopped,
    Paused,
}

/// User-control reason carried across workflow-agent attempts.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum WorkflowAgentAttemptReason {
    UserSkip,
    UserRetry,
    RetryLimitReached,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowStartedNotification {
    pub thread_id: String,
    pub run_id: String,
    /// Source run whose journal or checkpoint seeded this run, if any.
    pub resumed_from_run_id: Option<String>,
    pub name: String,
    pub phases: Vec<String>,
    pub args_digest: String,
    /// Unix timestamp in seconds when app-server observed the run starting.
    #[ts(type = "number")]
    pub started_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowPhaseChangedNotification {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub phase_index: u64,
    pub title: String,
    pub status: WorkflowPhaseStatus,
    /// Unix timestamp in seconds when app-server observed the phase boundary.
    #[ts(type = "number")]
    pub changed_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowGroupStartedNotification {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub group_id: u64,
    #[ts(type = "number | null")]
    pub parent_node_id: Option<u64>,
    pub kind: WorkflowGroupKind,
    #[ts(type = "number")]
    pub item_count: u64,
    /// Unix timestamp in seconds when app-server observed the group starting.
    #[ts(type = "number")]
    pub started_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowGroupCompletedNotification {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub group_id: u64,
    pub kind: WorkflowGroupKind,
    #[ts(type = "number")]
    pub item_count: u64,
    /// Unix timestamp in seconds when app-server observed the group completing.
    #[ts(type = "number")]
    pub completed_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentStartedNotification {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub node_id: u64,
    #[ts(type = "number")]
    pub attempt: u32,
    pub last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    #[ts(type = "number | null")]
    pub parent_node_id: Option<u64>,
    pub label: String,
    pub phase: Option<String>,
    pub model: String,
    pub effort: ReasoningEffort,
    /// Unix timestamp in seconds when app-server observed the agent starting.
    #[ts(type = "number")]
    pub started_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentBoundNotification {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub node_id: u64,
    #[ts(type = "number")]
    pub attempt: u32,
    /// Persistent child thread that clients can resume for workflow drill-in.
    pub child_thread_id: String,
    /// Unix timestamp in seconds when app-server observed the child binding.
    #[ts(type = "number")]
    pub bound_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentUpdatedNotification {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub node_id: u64,
    #[ts(type = "number")]
    pub attempt: u32,
    pub last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub token_usage: TokenUsageBreakdown,
    #[ts(type = "number")]
    pub tool_call_count: u64,
    /// Aggregate wall-clock duration across the initial attempt and every retry.
    #[ts(type = "number")]
    pub duration_ms: u64,
    /// Unix timestamp in seconds when app-server observed the counter update.
    #[ts(type = "number")]
    pub updated_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowAgentCompletedNotification {
    pub thread_id: String,
    pub run_id: String,
    #[ts(type = "number")]
    pub node_id: u64,
    #[ts(type = "number")]
    pub attempt: u32,
    pub last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub status: CollabAgentStatus,
    pub message: Option<String>,
    pub token_usage: TokenUsageBreakdown,
    #[ts(type = "number")]
    pub tool_call_count: u64,
    /// Aggregate wall-clock duration across the initial attempt and every retry.
    #[ts(type = "number")]
    pub duration_ms: u64,
    pub returned_null: bool,
    /// Unix timestamp in seconds when app-server observed the agent completing.
    #[ts(type = "number")]
    pub completed_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowLogNotification {
    pub thread_id: String,
    pub run_id: String,
    pub message: String,
    /// Unix timestamp in seconds when app-server observed the log entry.
    #[ts(type = "number")]
    pub emitted_at: i64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct WorkflowCompletedNotification {
    pub thread_id: String,
    pub run_id: String,
    pub status: CollabAgentStatus,
    pub message: Option<String>,
    /// Exact workflow-specific terminal reason, or `null` for a legacy event.
    pub terminal_reason: Option<WorkflowRunTerminalReason>,
    /// Weighted tokens consumed by the run.
    #[ts(type = "number")]
    pub spent: i64,
    /// Total weighted-token budget granted to the run, or `null` when unmetered.
    #[ts(type = "number | null")]
    pub total: Option<i64>,
    /// Unix timestamp in seconds when app-server observed the run completing.
    #[ts(type = "number")]
    pub completed_at: i64,
}

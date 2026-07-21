//! Core workflow progress events.
//!
//! These payloads stay transport-neutral: core emits them through [`EventMsg`], while app-server
//! maps them onto its versioned `workflow/*` notification surface.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

use crate::openai_models::ReasoningEffort as ReasoningEffortConfig;
use crate::protocol::AgentStatus;
use crate::protocol::EventMsg;
use crate::protocol::TokenUsage;

/// Workflow progress payload carried behind the stable [`EventMsg::Workflow`] seam.
///
/// Keeping the workflow family in a focused tagged union lets transport consumers handle one
/// top-level `EventMsg` variant while remaining exhaustive over workflow-specific evolution.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "event", rename_all = "snake_case")]
#[ts(tag = "event", rename_all = "snake_case")]
pub enum WorkflowEvent {
    RunBegin(WorkflowRunBeginEvent),
    RunEnd(WorkflowRunEndEvent),
    PhaseBegin(WorkflowPhaseBeginEvent),
    PhaseEnd(WorkflowPhaseEndEvent),
    GroupBegin(WorkflowGroupBeginEvent),
    GroupEnd(WorkflowGroupEndEvent),
    AgentBegin(WorkflowAgentBeginEvent),
    AgentBound(WorkflowAgentBoundEvent),
    AgentUpdated(WorkflowAgentUpdatedEvent),
    AgentEnd(WorkflowAgentEndEvent),
    Log(WorkflowLogEvent),
}

/// Execution semantics for a workflow group.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkflowGroupKind {
    /// Start every item before waiting for the group to finish.
    Parallel,
    /// Start items incrementally without an implicit completion barrier.
    Pipeline,
}

/// User control reason carried by one logical workflow-agent node across attempts.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkflowAgentAttemptReason {
    /// The prior attempt was skipped and this logical call settled to null.
    UserSkip,
    /// The prior attempt was cancelled so the same logical call could retry.
    UserRetry,
    /// A retry request reached the configured attempt limit, so no new child was spawned.
    RetryLimitReached,
}

/// Workflow-specific reason that a run reached its terminal state.
///
/// [`AgentStatus`] remains on [`WorkflowRunEndEvent`] as the backward-compatible coarse status.
/// This reason preserves run-control distinctions, notably checkpoint pause versus an unrelated
/// process interruption, without adding workflow-only variants to the global agent status.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "snake_case")]
#[ts(rename_all = "snake_case")]
pub enum WorkflowRunTerminalReason {
    Completed,
    Failed,
    Interrupted,
    Stopped,
    Paused,
}

/// Announces a workflow run and its full statically declared phase skeleton.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowRunBeginEvent {
    pub run_id: String,
    /// Source run whose journal or checkpoint seeded this fresh run, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub resumed_from_run_id: Option<String>,
    pub name: String,
    pub phases: Vec<String>,
    pub args_digest: String,
}

/// Reports the terminal state and token budget of a workflow run.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowRunEndEvent {
    pub run_id: String,
    /// Run-scoped terminal status. `Shutdown` denotes an explicit user stop;
    /// process/session interruption remains `Interrupted`.
    ///
    /// This intentionally reuses the established agent-status wire shape so
    /// existing workflow event consumers remain backward compatible.
    pub status: AgentStatus,
    /// Exact workflow terminal reason. Older persisted events omit this field and are interpreted
    /// from `status`; every newly emitted event supplies it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub terminal_reason: Option<WorkflowRunTerminalReason>,
    /// Weighted tokens consumed by the run.
    #[ts(type = "number")]
    pub spent: i64,
    /// Total weighted-token budget granted to the run, or `None` when unmetered.
    #[ts(type = "number | null")]
    pub total: Option<i64>,
}

/// Marks the beginning of one declared or dynamically appended phase.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowPhaseBeginEvent {
    pub run_id: String,
    /// Zero-based deterministic phase ordinal. Workflow phase counts are bounded well below the
    /// JavaScript safe-integer limit.
    #[ts(type = "number")]
    pub phase_index: u64,
    pub title: String,
}

/// Marks the end of one declared or dynamically appended phase.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowPhaseEndEvent {
    pub run_id: String,
    /// Zero-based deterministic phase ordinal. Workflow phase counts are bounded well below the
    /// JavaScript safe-integer limit.
    #[ts(type = "number")]
    pub phase_index: u64,
    pub title: String,
}

/// Marks the beginning of a parallel or pipeline group.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowGroupBeginEvent {
    pub run_id: String,
    /// Deterministic per-run topology ID, allocated from the counter shared with agent nodes.
    #[ts(type = "number")]
    pub group_id: u64,
    /// Parent group or agent ID from the same topology counter, or `None` at the run root.
    #[ts(type = "number | null")]
    pub parent_node_id: Option<u64>,
    pub kind: WorkflowGroupKind,
    /// Number of items admitted to the bounded group.
    #[ts(type = "number")]
    pub item_count: u64,
}

/// Marks the end of a parallel or pipeline group.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowGroupEndEvent {
    pub run_id: String,
    /// Deterministic per-run topology ID, allocated from the counter shared with agent nodes.
    #[ts(type = "number")]
    pub group_id: u64,
    pub kind: WorkflowGroupKind,
    /// Number of items admitted to the bounded group.
    #[ts(type = "number")]
    pub item_count: u64,
}

/// Announces a workflow-owned child agent.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowAgentBeginEvent {
    pub run_id: String,
    /// Deterministic per-run topology ID, allocated from the counter shared with groups.
    #[ts(type = "number")]
    pub node_id: u64,
    /// Zero-based live attempt generation. Omitted legacy events mean the initial attempt.
    #[serde(default)]
    #[ts(type = "number")]
    pub attempt: u32,
    /// Control reason that led to this generation, if it is a retry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    /// Parent group or agent ID from the same topology counter, or `None` at the run root.
    #[ts(type = "number | null")]
    pub parent_node_id: Option<u64>,
    pub label: String,
    /// Phase title active when the child was spawned.
    pub phase: Option<String>,
    /// Effective model after inheritance and role overrides.
    pub model: String,
    /// Effective reasoning effort after inheritance and role overrides.
    pub effort: ReasoningEffortConfig,
}

/// Binds a workflow topology node to its persistent child thread.
///
/// This follows [`WorkflowAgentBeginEvent`] once the child thread has been registered and precedes
/// all counter updates and the terminal agent event.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowAgentBoundEvent {
    pub run_id: String,
    /// Deterministic per-run node ordinal from [`WorkflowAgentBeginEvent`].
    #[ts(type = "number")]
    pub node_id: u64,
    /// Exact zero-based generation being bound.
    #[serde(default)]
    #[ts(type = "number")]
    pub attempt: u32,
    /// Persistent child thread that clients can resume for workflow drill-in.
    pub child_thread_id: String,
}

/// Reports the live counters needed to redraw one workflow agent leaf.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowAgentUpdatedEvent {
    pub run_id: String,
    /// Deterministic per-run node ordinal from [`WorkflowAgentBeginEvent`].
    #[ts(type = "number")]
    pub node_id: u64,
    /// Exact zero-based generation that produced this aggregate snapshot.
    #[serde(default)]
    #[ts(type = "number")]
    pub attempt: u32,
    /// Most recent selected-attempt control reason for this logical node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub token_usage: TokenUsage,
    #[ts(type = "number")]
    pub tool_call_count: u64,
    /// Aggregate wall-clock duration across the initial attempt and every retry.
    #[serde(default)]
    #[ts(type = "number")]
    pub duration_ms: u64,
}

/// Reports the terminal state and final counters of one workflow agent.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowAgentEndEvent {
    pub run_id: String,
    /// Deterministic per-run node ordinal from [`WorkflowAgentBeginEvent`].
    #[ts(type = "number")]
    pub node_id: u64,
    /// Exact zero-based generation that produced this one logical terminal event.
    #[serde(default)]
    #[ts(type = "number")]
    pub attempt: u32,
    /// Most recent selected-attempt control reason, including user skip or retry exhaustion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    pub status: AgentStatus,
    pub token_usage: TokenUsage,
    #[ts(type = "number")]
    pub tool_call_count: u64,
    /// Aggregate wall-clock duration across the initial attempt and every retry.
    #[serde(default)]
    #[ts(type = "number")]
    pub duration_ms: u64,
    pub returned_null: bool,
}

/// Carries one workflow-authored `log()` narration line.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
pub struct WorkflowLogEvent {
    pub run_id: String,
    pub message: String,
}

macro_rules! impl_workflow_event_from {
    ($event:ty, $variant:ident) => {
        impl From<$event> for WorkflowEvent {
            fn from(event: $event) -> Self {
                Self::$variant(event)
            }
        }

        impl From<$event> for EventMsg {
            fn from(event: $event) -> Self {
                Self::Workflow(WorkflowEvent::$variant(event))
            }
        }
    };
}

impl From<WorkflowEvent> for EventMsg {
    fn from(event: WorkflowEvent) -> Self {
        Self::Workflow(event)
    }
}

impl_workflow_event_from!(WorkflowRunBeginEvent, RunBegin);
impl_workflow_event_from!(WorkflowRunEndEvent, RunEnd);
impl_workflow_event_from!(WorkflowPhaseBeginEvent, PhaseBegin);
impl_workflow_event_from!(WorkflowPhaseEndEvent, PhaseEnd);
impl_workflow_event_from!(WorkflowGroupBeginEvent, GroupBegin);
impl_workflow_event_from!(WorkflowGroupEndEvent, GroupEnd);
impl_workflow_event_from!(WorkflowAgentBeginEvent, AgentBegin);
impl_workflow_event_from!(WorkflowAgentBoundEvent, AgentBound);
impl_workflow_event_from!(WorkflowAgentUpdatedEvent, AgentUpdated);
impl_workflow_event_from!(WorkflowAgentEndEvent, AgentEnd);
impl_workflow_event_from!(WorkflowLogEvent, Log);

#[cfg(test)]
#[path = "workflow_events_tests.rs"]
mod tests;

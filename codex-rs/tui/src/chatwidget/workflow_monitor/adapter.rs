//! App-server notification adapter for the renderer-neutral workflow reducer.

use super::MAX_PHASES_PER_RUN;
use super::bound_run_id;
use super::bound_text;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_app_server_protocol::WorkflowAgentAttemptReason as AppWorkflowAgentAttemptReason;
use codex_app_server_protocol::WorkflowAgentBoundNotification;
use codex_app_server_protocol::WorkflowAgentCompletedNotification;
use codex_app_server_protocol::WorkflowAgentStartedNotification;
use codex_app_server_protocol::WorkflowAgentUpdatedNotification;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowGroupCompletedNotification;
use codex_app_server_protocol::WorkflowGroupKind as AppWorkflowGroupKind;
use codex_app_server_protocol::WorkflowGroupStartedNotification;
use codex_app_server_protocol::WorkflowLogNotification;
use codex_app_server_protocol::WorkflowPhaseChangedNotification;
use codex_app_server_protocol::WorkflowPhaseStatus;
use codex_app_server_protocol::WorkflowRunTerminalReason as AppWorkflowRunTerminalReason;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_protocol::ThreadId;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;

const MAX_LABEL_CHARS: usize = 96;
const MAX_MODEL_CHARS: usize = 96;
const MAX_NAME_CHARS: usize = 96;
const MAX_PHASE_TITLE_CHARS: usize = 96;
const MAX_STATUS_MESSAGE_CHARS: usize = 240;
const MAX_ARGS_DIGEST_CHARS: usize = 160;
const MAX_CHILD_THREAD_ID_CHARS: usize = 64;

pub(in crate::chatwidget) enum WorkflowNotification {
    Started(WorkflowStartedNotification),
    PhaseChanged(WorkflowPhaseChangedNotification),
    GroupStarted(WorkflowGroupStartedNotification),
    GroupCompleted(WorkflowGroupCompletedNotification),
    AgentStarted(WorkflowAgentStartedNotification),
    AgentBound(WorkflowAgentBoundNotification),
    AgentUpdated(WorkflowAgentUpdatedNotification),
    AgentCompleted(WorkflowAgentCompletedNotification),
    Log(WorkflowLogNotification),
    Completed(WorkflowCompletedNotification),
}

impl WorkflowNotification {
    /// Returns the unmodified run name before the renderer projection applies display bounds.
    pub(super) fn durable_name(&self) -> Option<&str> {
        match self {
            Self::Started(notification) => Some(&notification.name),
            Self::PhaseChanged(_)
            | Self::GroupStarted(_)
            | Self::GroupCompleted(_)
            | Self::AgentStarted(_)
            | Self::AgentBound(_)
            | Self::AgentUpdated(_)
            | Self::AgentCompleted(_)
            | Self::Log(_)
            | Self::Completed(_) => None,
        }
    }

    pub(super) fn thread_id(&self) -> &str {
        match self {
            Self::Started(notification) => &notification.thread_id,
            Self::PhaseChanged(notification) => &notification.thread_id,
            Self::GroupStarted(notification) => &notification.thread_id,
            Self::GroupCompleted(notification) => &notification.thread_id,
            Self::AgentStarted(notification) => &notification.thread_id,
            Self::AgentBound(notification) => &notification.thread_id,
            Self::AgentUpdated(notification) => &notification.thread_id,
            Self::AgentCompleted(notification) => &notification.thread_id,
            Self::Log(notification) => &notification.thread_id,
            Self::Completed(notification) => &notification.thread_id,
        }
    }

    /// Returns the app-server observation timestamp carried by this notification.
    pub(super) fn observed_at(&self) -> i64 {
        match self {
            Self::Started(notification) => notification.started_at,
            Self::PhaseChanged(notification) => notification.changed_at,
            Self::GroupStarted(notification) => notification.started_at,
            Self::GroupCompleted(notification) => notification.completed_at,
            Self::AgentStarted(notification) => notification.started_at,
            Self::AgentBound(notification) => notification.bound_at,
            Self::AgentUpdated(notification) => notification.updated_at,
            Self::AgentCompleted(notification) => notification.completed_at,
            Self::Log(notification) => notification.emitted_at,
            Self::Completed(notification) => notification.completed_at,
        }
    }

    pub(super) fn into_event(self) -> WorkflowEvent {
        match self {
            Self::Started(notification) => WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
                run_id: bound_run_id(notification.run_id),
                resumed_from_run_id: notification.resumed_from_run_id.map(bound_run_id),
                name: bound_text(notification.name, MAX_NAME_CHARS),
                phases: notification
                    .phases
                    .into_iter()
                    .take(MAX_PHASES_PER_RUN)
                    .map(|phase| bound_text(phase, MAX_PHASE_TITLE_CHARS))
                    .collect(),
                args_digest: bound_text(notification.args_digest, MAX_ARGS_DIGEST_CHARS),
            }),
            Self::PhaseChanged(notification) => match notification.status {
                WorkflowPhaseStatus::Active => WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
                    run_id: bound_run_id(notification.run_id),
                    phase_index: notification.phase_index,
                    title: bound_text(notification.title, MAX_PHASE_TITLE_CHARS),
                }),
                WorkflowPhaseStatus::Completed => WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
                    run_id: bound_run_id(notification.run_id),
                    phase_index: notification.phase_index,
                    title: bound_text(notification.title, MAX_PHASE_TITLE_CHARS),
                }),
            },
            Self::GroupStarted(notification) => {
                WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
                    run_id: bound_run_id(notification.run_id),
                    group_id: notification.group_id,
                    parent_node_id: notification.parent_node_id,
                    kind: group_kind(notification.kind),
                    item_count: notification.item_count,
                })
            }
            Self::GroupCompleted(notification) => WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
                run_id: bound_run_id(notification.run_id),
                group_id: notification.group_id,
                kind: group_kind(notification.kind),
                item_count: notification.item_count,
            }),
            Self::AgentStarted(notification) => {
                WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
                    run_id: bound_run_id(notification.run_id),
                    node_id: notification.node_id,
                    attempt: notification.attempt,
                    last_attempt_reason: notification.last_attempt_reason.map(agent_attempt_reason),
                    parent_node_id: notification.parent_node_id,
                    label: bound_text(notification.label, MAX_LABEL_CHARS),
                    phase: notification
                        .phase
                        .map(|phase| bound_text(phase, MAX_PHASE_TITLE_CHARS)),
                    model: bound_text(notification.model, MAX_MODEL_CHARS),
                    effort: notification.effort,
                })
            }
            Self::AgentBound(notification) => WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
                run_id: bound_run_id(notification.run_id),
                node_id: notification.node_id,
                attempt: notification.attempt,
                child_thread_id: bound_child_thread_id(notification.child_thread_id),
            }),
            Self::AgentUpdated(notification) => {
                WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
                    run_id: bound_run_id(notification.run_id),
                    node_id: notification.node_id,
                    attempt: notification.attempt,
                    last_attempt_reason: notification.last_attempt_reason.map(agent_attempt_reason),
                    token_usage: token_usage(notification.token_usage),
                    tool_call_count: notification.tool_call_count,
                    duration_ms: notification.duration_ms,
                })
            }
            Self::AgentCompleted(notification) => WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
                run_id: bound_run_id(notification.run_id),
                node_id: notification.node_id,
                attempt: notification.attempt,
                last_attempt_reason: notification.last_attempt_reason.map(agent_attempt_reason),
                status: agent_status(notification.status, notification.message),
                token_usage: token_usage(notification.token_usage),
                tool_call_count: notification.tool_call_count,
                duration_ms: notification.duration_ms,
                returned_null: notification.returned_null,
            }),
            Self::Log(notification) => WorkflowEvent::Log(WorkflowLogEvent {
                run_id: bound_run_id(notification.run_id),
                message: notification.message,
            }),
            Self::Completed(notification) => WorkflowEvent::RunEnd(WorkflowRunEndEvent {
                run_id: bound_run_id(notification.run_id),
                status: agent_status(notification.status, notification.message),
                terminal_reason: notification.terminal_reason.map(run_terminal_reason),
                spent: notification.spent,
                total: notification.total,
            }),
        }
    }
}

fn bound_child_thread_id(child_thread_id: String) -> String {
    ThreadId::from_string(&child_thread_id)
        .map(|thread_id| thread_id.to_string())
        .unwrap_or_else(|_| bound_text(child_thread_id, MAX_CHILD_THREAD_ID_CHARS))
}

fn run_terminal_reason(reason: AppWorkflowRunTerminalReason) -> WorkflowRunTerminalReason {
    match reason {
        AppWorkflowRunTerminalReason::Completed => WorkflowRunTerminalReason::Completed,
        AppWorkflowRunTerminalReason::Failed => WorkflowRunTerminalReason::Failed,
        AppWorkflowRunTerminalReason::Interrupted => WorkflowRunTerminalReason::Interrupted,
        AppWorkflowRunTerminalReason::Stopped => WorkflowRunTerminalReason::Stopped,
        AppWorkflowRunTerminalReason::Paused => WorkflowRunTerminalReason::Paused,
    }
}

fn agent_attempt_reason(reason: AppWorkflowAgentAttemptReason) -> WorkflowAgentAttemptReason {
    match reason {
        AppWorkflowAgentAttemptReason::UserSkip => WorkflowAgentAttemptReason::UserSkip,
        AppWorkflowAgentAttemptReason::UserRetry => WorkflowAgentAttemptReason::UserRetry,
        AppWorkflowAgentAttemptReason::RetryLimitReached => {
            WorkflowAgentAttemptReason::RetryLimitReached
        }
    }
}

fn group_kind(kind: AppWorkflowGroupKind) -> WorkflowGroupKind {
    match kind {
        AppWorkflowGroupKind::Parallel => WorkflowGroupKind::Parallel,
        AppWorkflowGroupKind::Pipeline => WorkflowGroupKind::Pipeline,
    }
}

fn agent_status(status: CollabAgentStatus, message: Option<String>) -> AgentStatus {
    let message = message.map(|message| bound_text(message, MAX_STATUS_MESSAGE_CHARS));
    match status {
        CollabAgentStatus::PendingInit => AgentStatus::PendingInit,
        CollabAgentStatus::Running => AgentStatus::Running,
        CollabAgentStatus::Interrupted => AgentStatus::Interrupted,
        CollabAgentStatus::Completed => AgentStatus::Completed(message),
        CollabAgentStatus::Errored => AgentStatus::Errored(message.unwrap_or_default()),
        CollabAgentStatus::Shutdown => AgentStatus::Shutdown,
        CollabAgentStatus::NotFound => AgentStatus::NotFound,
    }
}

fn token_usage(usage: TokenUsageBreakdown) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        cached_input_tokens: usage.cached_input_tokens,
        output_tokens: usage.output_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
        total_tokens: usage.total_tokens,
    }
}

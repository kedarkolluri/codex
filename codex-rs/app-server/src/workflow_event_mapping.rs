use codex_app_server_protocol::CollabAgentState;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WorkflowAgentAttemptReason;
use codex_app_server_protocol::WorkflowAgentBoundNotification;
use codex_app_server_protocol::WorkflowAgentCompletedNotification;
use codex_app_server_protocol::WorkflowAgentStartedNotification;
use codex_app_server_protocol::WorkflowAgentUpdatedNotification;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowGroupCompletedNotification;
use codex_app_server_protocol::WorkflowGroupKind;
use codex_app_server_protocol::WorkflowGroupStartedNotification;
use codex_app_server_protocol::WorkflowLogNotification;
use codex_app_server_protocol::WorkflowPhaseChangedNotification;
use codex_app_server_protocol::WorkflowPhaseStatus;
use codex_app_server_protocol::WorkflowRunTerminalReason;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_protocol::protocol::WorkflowAgentAttemptReason as CoreWorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupKind as CoreWorkflowGroupKind;
use codex_protocol::protocol::WorkflowRunTerminalReason as CoreWorkflowRunTerminalReason;

pub(crate) fn workflow_event_to_server_notification(
    thread_id: &str,
    event: WorkflowEvent,
    observed_at: i64,
) -> ServerNotification {
    match event {
        WorkflowEvent::RunBegin(event) => {
            ServerNotification::WorkflowStarted(WorkflowStartedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                resumed_from_run_id: event.resumed_from_run_id,
                name: event.name,
                phases: event.phases,
                args_digest: event.args_digest,
                started_at: observed_at,
            })
        }
        WorkflowEvent::RunEnd(event) => {
            let CollabAgentState { status, message } = event.status.into();
            ServerNotification::WorkflowCompleted(WorkflowCompletedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                status,
                message,
                terminal_reason: event.terminal_reason.map(workflow_run_terminal_reason),
                spent: event.spent,
                total: event.total,
                completed_at: observed_at,
            })
        }
        WorkflowEvent::PhaseBegin(event) => {
            ServerNotification::WorkflowPhaseChanged(WorkflowPhaseChangedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                phase_index: event.phase_index,
                title: event.title,
                status: WorkflowPhaseStatus::Active,
                changed_at: observed_at,
            })
        }
        WorkflowEvent::PhaseEnd(event) => {
            ServerNotification::WorkflowPhaseChanged(WorkflowPhaseChangedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                phase_index: event.phase_index,
                title: event.title,
                status: WorkflowPhaseStatus::Completed,
                changed_at: observed_at,
            })
        }
        WorkflowEvent::GroupBegin(event) => {
            ServerNotification::WorkflowGroupStarted(WorkflowGroupStartedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                group_id: event.group_id,
                parent_node_id: event.parent_node_id,
                kind: workflow_group_kind(event.kind),
                item_count: event.item_count,
                started_at: observed_at,
            })
        }
        WorkflowEvent::GroupEnd(event) => {
            ServerNotification::WorkflowGroupCompleted(WorkflowGroupCompletedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                group_id: event.group_id,
                kind: workflow_group_kind(event.kind),
                item_count: event.item_count,
                completed_at: observed_at,
            })
        }
        WorkflowEvent::AgentBegin(event) => {
            ServerNotification::WorkflowAgentStarted(WorkflowAgentStartedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                node_id: event.node_id,
                attempt: event.attempt,
                last_attempt_reason: event.last_attempt_reason.map(workflow_agent_attempt_reason),
                parent_node_id: event.parent_node_id,
                label: event.label,
                phase: event.phase,
                model: event.model,
                effort: event.effort,
                started_at: observed_at,
            })
        }
        WorkflowEvent::AgentBound(event) => {
            ServerNotification::WorkflowAgentBound(WorkflowAgentBoundNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                node_id: event.node_id,
                attempt: event.attempt,
                child_thread_id: event.child_thread_id,
                bound_at: observed_at,
            })
        }
        WorkflowEvent::AgentUpdated(event) => {
            ServerNotification::WorkflowAgentUpdated(WorkflowAgentUpdatedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                node_id: event.node_id,
                attempt: event.attempt,
                last_attempt_reason: event.last_attempt_reason.map(workflow_agent_attempt_reason),
                token_usage: event.token_usage.into(),
                tool_call_count: event.tool_call_count,
                duration_ms: event.duration_ms,
                updated_at: observed_at,
            })
        }
        WorkflowEvent::AgentEnd(event) => {
            let CollabAgentState { status, message } = event.status.into();
            ServerNotification::WorkflowAgentCompleted(WorkflowAgentCompletedNotification {
                thread_id: thread_id.to_string(),
                run_id: event.run_id,
                node_id: event.node_id,
                attempt: event.attempt,
                last_attempt_reason: event.last_attempt_reason.map(workflow_agent_attempt_reason),
                status,
                message,
                token_usage: event.token_usage.into(),
                tool_call_count: event.tool_call_count,
                duration_ms: event.duration_ms,
                returned_null: event.returned_null,
                completed_at: observed_at,
            })
        }
        WorkflowEvent::Log(event) => ServerNotification::WorkflowLog(WorkflowLogNotification {
            thread_id: thread_id.to_string(),
            run_id: event.run_id,
            message: event.message,
            emitted_at: observed_at,
        }),
    }
}

fn workflow_group_kind(kind: CoreWorkflowGroupKind) -> WorkflowGroupKind {
    match kind {
        CoreWorkflowGroupKind::Parallel => WorkflowGroupKind::Parallel,
        CoreWorkflowGroupKind::Pipeline => WorkflowGroupKind::Pipeline,
    }
}

fn workflow_run_terminal_reason(
    reason: CoreWorkflowRunTerminalReason,
) -> WorkflowRunTerminalReason {
    match reason {
        CoreWorkflowRunTerminalReason::Completed => WorkflowRunTerminalReason::Completed,
        CoreWorkflowRunTerminalReason::Failed => WorkflowRunTerminalReason::Failed,
        CoreWorkflowRunTerminalReason::Interrupted => WorkflowRunTerminalReason::Interrupted,
        CoreWorkflowRunTerminalReason::Stopped => WorkflowRunTerminalReason::Stopped,
        CoreWorkflowRunTerminalReason::Paused => WorkflowRunTerminalReason::Paused,
    }
}

fn workflow_agent_attempt_reason(
    reason: CoreWorkflowAgentAttemptReason,
) -> WorkflowAgentAttemptReason {
    match reason {
        CoreWorkflowAgentAttemptReason::UserSkip => WorkflowAgentAttemptReason::UserSkip,
        CoreWorkflowAgentAttemptReason::UserRetry => WorkflowAgentAttemptReason::UserRetry,
        CoreWorkflowAgentAttemptReason::RetryLimitReached => {
            WorkflowAgentAttemptReason::RetryLimitReached
        }
    }
}

#[cfg(test)]
#[path = "workflow_event_mapping_tests.rs"]
mod tests;

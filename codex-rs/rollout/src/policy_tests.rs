use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use pretty_assertions::assert_eq;

use super::should_persist_event_msg;

fn workflow_events() -> Vec<EventMsg> {
    let run_id = "run-1".to_string();
    vec![
        WorkflowRunBeginEvent {
            run_id: run_id.clone(),
            resumed_from_run_id: None,
            name: "review".to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "digest".to_string(),
        }
        .into(),
        WorkflowRunEndEvent {
            run_id: run_id.clone(),
            status: AgentStatus::Completed(None),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 5,
            total: Some(10),
        }
        .into(),
        WorkflowPhaseBeginEvent {
            run_id: run_id.clone(),
            phase_index: 0,
            title: "inspect".to_string(),
        }
        .into(),
        WorkflowPhaseEndEvent {
            run_id: run_id.clone(),
            phase_index: 0,
            title: "inspect".to_string(),
        }
        .into(),
        WorkflowGroupBeginEvent {
            run_id: run_id.clone(),
            group_id: 1,
            parent_node_id: None,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }
        .into(),
        WorkflowGroupEndEvent {
            run_id: run_id.clone(),
            group_id: 1,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }
        .into(),
        WorkflowAgentBeginEvent {
            run_id: run_id.clone(),
            node_id: 2,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: Some(1),
            label: "reviewer".to_string(),
            phase: Some("inspect".to_string()),
            model: "test-model".to_string(),
            effort: ReasoningEffort::Medium,
        }
        .into(),
        WorkflowAgentUpdatedEvent {
            run_id: run_id.clone(),
            node_id: 2,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: TokenUsage::default(),
            tool_call_count: 1,
            duration_ms: 0,
        }
        .into(),
        WorkflowAgentEndEvent {
            run_id: run_id.clone(),
            node_id: 2,
            attempt: 0,
            last_attempt_reason: None,
            status: AgentStatus::Completed(None),
            token_usage: TokenUsage::default(),
            tool_call_count: 1,
            duration_ms: 0,
            returned_null: false,
        }
        .into(),
        WorkflowLogEvent {
            run_id,
            message: "done".to_string(),
        }
        .into(),
    ]
}

#[test]
fn workflow_progress_is_persisted_in_every_history_mode() {
    let events = workflow_events();
    let actual = events
        .iter()
        .map(|event| {
            (
                should_persist_event_msg(event, ThreadHistoryMode::Legacy),
                should_persist_event_msg(event, ThreadHistoryMode::Paginated),
            )
        })
        .collect::<Vec<_>>();

    assert_eq!(actual, vec![(true, true); events.len()]);
}

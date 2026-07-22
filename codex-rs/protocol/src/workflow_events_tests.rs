use anyhow::Result;
use pretty_assertions::assert_eq;

use super::*;

fn token_usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens - 5,
        cached_input_tokens: 2,
        cache_write_input_tokens: 1,
        output_tokens: 5,
        reasoning_output_tokens: 3,
        total_tokens,
    }
}

macro_rules! assert_event_round_trip {
    ($event:expr, $variant:ident, $wire_type:literal) => {{
        let expected = $event;
        let encoded = serde_json::to_vec(&EventMsg::from(expected.clone()))?;
        let encoded_value = serde_json::from_slice::<serde_json::Value>(&encoded)?;
        assert_eq!(encoded_value["type"], "workflow");
        assert_eq!(encoded_value["event"], $wire_type);
        let decoded = serde_json::from_slice::<EventMsg>(&encoded)?;
        let EventMsg::Workflow(WorkflowEvent::$variant(actual)) = decoded else {
            panic!("expected {} after round trip", stringify!($variant));
        };
        assert_eq!(actual, expected);
    }};
}

#[test]
fn every_workflow_event_round_trips_with_full_payload_equality() -> Result<()> {
    assert_event_round_trip!(
        WorkflowRunBeginEvent {
            run_id: "run-7".to_string(),
            resumed_from_run_id: Some("run-6".to_string()),
            name: "release-audit".to_string(),
            phases: vec!["inventory".to_string(), "review".to_string()],
            args_digest: "blake3:args".to_string(),
        },
        RunBegin,
        "run_begin"
    );
    assert_event_round_trip!(
        WorkflowRunEndEvent {
            run_id: "run-7".to_string(),
            status: AgentStatus::Completed(Some("ready".to_string())),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 2_048,
            total: Some(8_192),
        },
        RunEnd,
        "run_end"
    );
    assert_event_round_trip!(
        WorkflowPhaseBeginEvent {
            run_id: "run-7".to_string(),
            phase_index: 1,
            title: "review".to_string(),
        },
        PhaseBegin,
        "phase_begin"
    );
    assert_event_round_trip!(
        WorkflowPhaseEndEvent {
            run_id: "run-7".to_string(),
            phase_index: 1,
            title: "review".to_string(),
        },
        PhaseEnd,
        "phase_end"
    );
    assert_event_round_trip!(
        WorkflowGroupBeginEvent {
            run_id: "run-7".to_string(),
            group_id: 2,
            parent_node_id: Some(1),
            kind: WorkflowGroupKind::Parallel,
            item_count: 3,
        },
        GroupBegin,
        "group_begin"
    );
    assert_event_round_trip!(
        WorkflowGroupEndEvent {
            run_id: "run-7".to_string(),
            group_id: 3,
            kind: WorkflowGroupKind::Pipeline,
            item_count: 4,
        },
        GroupEnd,
        "group_end"
    );
    assert_event_round_trip!(
        WorkflowAgentBeginEvent {
            run_id: "run-7".to_string(),
            node_id: 4,
            attempt: 1,
            last_attempt_reason: Some(WorkflowAgentAttemptReason::UserRetry),
            parent_node_id: Some(2),
            label: "review-api".to_string(),
            phase: Some("review".to_string()),
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffortConfig::High,
        },
        AgentBegin,
        "agent_begin"
    );
    assert_event_round_trip!(
        WorkflowAgentBoundEvent {
            run_id: "run-7".to_string(),
            node_id: 4,
            attempt: 1,
            child_thread_id: "019b0214-7c3f-7d80-bd83-88cb759c24a7".to_string(),
        },
        AgentBound,
        "agent_bound"
    );
    assert_event_round_trip!(
        WorkflowAgentUpdatedEvent {
            run_id: "run-7".to_string(),
            node_id: 4,
            attempt: 2,
            last_attempt_reason: Some(WorkflowAgentAttemptReason::UserSkip),
            token_usage: token_usage(/*total_tokens*/ 21),
            tool_call_count: 2,
            duration_ms: 500,
        },
        AgentUpdated,
        "agent_updated"
    );
    assert_event_round_trip!(
        WorkflowAgentEndEvent {
            run_id: "run-7".to_string(),
            node_id: 4,
            attempt: 5,
            last_attempt_reason: Some(WorkflowAgentAttemptReason::RetryLimitReached),
            status: AgentStatus::Errored("model disconnected".to_string()),
            token_usage: token_usage(/*total_tokens*/ 34),
            tool_call_count: 3,
            duration_ms: 900,
            returned_null: true,
        },
        AgentEnd,
        "agent_end"
    );
    assert_event_round_trip!(
        WorkflowLogEvent {
            run_id: "run-7".to_string(),
            message: "review complete".to_string(),
        },
        Log,
        "log"
    );
    Ok(())
}

#[test]
fn run_end_budget_total_distinguishes_unmetered_from_zero_limit() -> Result<()> {
    let event = |total| WorkflowRunEndEvent {
        run_id: "run-budget".to_string(),
        status: AgentStatus::Completed(None),
        terminal_reason: Some(WorkflowRunTerminalReason::Completed),
        spent: 0,
        total,
    };
    for (total, expected_wire_total) in [(None, serde_json::Value::Null), (Some(0), 0.into())] {
        let expected = event(total);
        let message = EventMsg::from(expected.clone());
        let encoded = serde_json::to_value(&message)?;
        assert_eq!(encoded["total"], expected_wire_total);
        let EventMsg::Workflow(WorkflowEvent::RunEnd(decoded)) =
            serde_json::from_value::<EventMsg>(encoded)?
        else {
            panic!("expected workflow run end")
        };
        assert_eq!(decoded, expected);
    }
    Ok(())
}

#[test]
fn run_end_user_stop_uses_the_existing_shutdown_wire_status() -> Result<()> {
    let expected = WorkflowRunEndEvent {
        run_id: "run-stopped".to_string(),
        status: AgentStatus::Shutdown,
        terminal_reason: Some(WorkflowRunTerminalReason::Stopped),
        spent: 42,
        total: Some(100),
    };

    let encoded = serde_json::to_value(EventMsg::from(expected.clone()))?;
    assert_eq!(encoded["status"], "shutdown");
    assert_eq!(encoded["terminal_reason"], "stopped");
    let EventMsg::Workflow(WorkflowEvent::RunEnd(decoded)) =
        serde_json::from_value::<EventMsg>(encoded)?
    else {
        panic!("expected workflow run end")
    };
    assert_eq!(decoded, expected);
    Ok(())
}

#[test]
fn run_end_checkpoint_pause_uses_the_existing_interrupted_wire_status() -> Result<()> {
    let expected = WorkflowRunEndEvent {
        run_id: "run-paused".to_string(),
        status: AgentStatus::Interrupted,
        terminal_reason: Some(WorkflowRunTerminalReason::Paused),
        spent: 42,
        total: Some(100),
    };

    let encoded = serde_json::to_value(EventMsg::from(expected.clone()))?;
    assert_eq!(encoded["status"], "interrupted");
    assert_eq!(encoded["terminal_reason"], "paused");
    let EventMsg::Workflow(WorkflowEvent::RunEnd(decoded)) =
        serde_json::from_value::<EventMsg>(encoded)?
    else {
        panic!("expected workflow run end")
    };
    assert_eq!(decoded, expected);
    Ok(())
}

#[test]
fn omitted_additive_fields_decode_to_legacy_defaults() -> Result<()> {
    let run_begin = serde_json::json!({
        "type": "workflow",
        "event": "run_begin",
        "run_id": "run-legacy",
        "name": "legacy",
        "phases": [],
        "args_digest": "digest"
    });
    let EventMsg::Workflow(WorkflowEvent::RunBegin(actual)) =
        serde_json::from_value::<EventMsg>(run_begin)?
    else {
        panic!("expected workflow run begin")
    };
    assert_eq!(
        actual,
        WorkflowRunBeginEvent {
            run_id: "run-legacy".to_string(),
            resumed_from_run_id: None,
            name: "legacy".to_string(),
            phases: Vec::new(),
            args_digest: "digest".to_string(),
        }
    );

    let run_end = serde_json::json!({
        "type": "workflow",
        "event": "run_end",
        "run_id": "run-legacy",
        "status": "interrupted",
        "spent": 0,
        "total": null
    });
    let EventMsg::Workflow(WorkflowEvent::RunEnd(actual)) =
        serde_json::from_value::<EventMsg>(run_end)?
    else {
        panic!("expected workflow run end")
    };
    assert_eq!(
        actual,
        WorkflowRunEndEvent {
            run_id: "run-legacy".to_string(),
            status: AgentStatus::Interrupted,
            terminal_reason: None,
            spent: 0,
            total: None,
        }
    );

    let agent_begin = serde_json::json!({
        "type": "workflow",
        "event": "agent_begin",
        "run_id": "run-legacy",
        "node_id": 1,
        "parent_node_id": null,
        "label": "agent",
        "phase": null,
        "model": "model",
        "effort": "medium"
    });
    let EventMsg::Workflow(WorkflowEvent::AgentBegin(actual)) =
        serde_json::from_value::<EventMsg>(agent_begin)?
    else {
        panic!("expected workflow agent begin")
    };
    assert_eq!(
        actual,
        WorkflowAgentBeginEvent {
            run_id: "run-legacy".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: None,
            label: "agent".to_string(),
            phase: None,
            model: "model".to_string(),
            effort: ReasoningEffortConfig::Medium,
        }
    );

    let agent_bound = serde_json::json!({
        "type": "workflow",
        "event": "agent_bound",
        "run_id": "run-legacy",
        "node_id": 1,
        "child_thread_id": "019b0214-7c3f-7d80-bd83-88cb759c24a7"
    });
    let EventMsg::Workflow(WorkflowEvent::AgentBound(actual)) =
        serde_json::from_value::<EventMsg>(agent_bound)?
    else {
        panic!("expected workflow agent bound")
    };
    assert_eq!(
        actual,
        WorkflowAgentBoundEvent {
            run_id: "run-legacy".to_string(),
            node_id: 1,
            attempt: 0,
            child_thread_id: "019b0214-7c3f-7d80-bd83-88cb759c24a7".to_string(),
        }
    );

    let agent_updated = serde_json::json!({
        "type": "workflow",
        "event": "agent_updated",
        "run_id": "run-legacy",
        "node_id": 1,
        "token_usage": token_usage(/*total_tokens*/ 10),
        "tool_call_count": 2
    });
    let EventMsg::Workflow(WorkflowEvent::AgentUpdated(actual)) =
        serde_json::from_value::<EventMsg>(agent_updated)?
    else {
        panic!("expected workflow agent update")
    };
    assert_eq!(
        actual,
        WorkflowAgentUpdatedEvent {
            run_id: "run-legacy".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: token_usage(/*total_tokens*/ 10),
            tool_call_count: 2,
            duration_ms: 0,
        }
    );

    let agent_end = serde_json::json!({
        "type": "workflow",
        "event": "agent_end",
        "run_id": "run-legacy",
        "node_id": 1,
        "status": "shutdown",
        "token_usage": token_usage(/*total_tokens*/ 10),
        "tool_call_count": 2,
        "returned_null": false
    });
    let EventMsg::Workflow(WorkflowEvent::AgentEnd(actual)) =
        serde_json::from_value::<EventMsg>(agent_end)?
    else {
        panic!("expected workflow agent end")
    };
    assert_eq!(
        actual,
        WorkflowAgentEndEvent {
            run_id: "run-legacy".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            status: AgentStatus::Shutdown,
            token_usage: token_usage(/*total_tokens*/ 10),
            tool_call_count: 2,
            duration_ms: 0,
            returned_null: false,
        }
    );
    Ok(())
}

use codex_app_server_protocol::CollabAgentStatus;
use codex_protocol::openai_models::ReasoningEffort;
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
use pretty_assertions::assert_eq;
use serde_json::json;

use super::workflow_event_to_server_notification;

const OBSERVED_AT: i64 = 1_784_400_000;

fn token_usage() -> TokenUsage {
    TokenUsage {
        input_tokens: 13,
        cached_input_tokens: 3,
        output_tokens: 8,
        reasoning_output_tokens: 5,
        total_tokens: 21,
    }
}

#[test]
fn maps_every_workflow_event_to_the_versioned_notification_contract() {
    let events = vec![
        WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: "run-7".to_string(),
            resumed_from_run_id: Some("run-paused".to_string()),
            name: "release-audit".to_string(),
            phases: vec!["inventory".to_string(), "review".to_string()],
            args_digest: "blake3:args".to_string(),
        }),
        WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: "run-7".to_string(),
            phase_index: 1,
            title: "review".to_string(),
        }),
        WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
            run_id: "run-7".to_string(),
            phase_index: 1,
            title: "review".to_string(),
        }),
        WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: "run-7".to_string(),
            group_id: 2,
            parent_node_id: None,
            kind: WorkflowGroupKind::Parallel,
            item_count: 3,
        }),
        WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
            run_id: "run-7".to_string(),
            group_id: 2,
            kind: WorkflowGroupKind::Parallel,
            item_count: 3,
        }),
        WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: "run-7".to_string(),
            node_id: 3,
            attempt: 2,
            last_attempt_reason: Some(WorkflowAgentAttemptReason::UserRetry),
            parent_node_id: Some(2),
            label: "review-api".to_string(),
            phase: Some("review".to_string()),
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffort::High,
        }),
        WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
            run_id: "run-7".to_string(),
            node_id: 3,
            attempt: 2,
            child_thread_id: "019b0214-7c3f-7d80-bd83-88cb759c24a7".to_string(),
        }),
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: "run-7".to_string(),
            node_id: 3,
            attempt: 2,
            last_attempt_reason: Some(WorkflowAgentAttemptReason::UserRetry),
            token_usage: token_usage(),
            tool_call_count: 2,
            duration_ms: 1_234,
        }),
        WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id: "run-7".to_string(),
            node_id: 3,
            attempt: 5,
            last_attempt_reason: Some(WorkflowAgentAttemptReason::RetryLimitReached),
            status: AgentStatus::Errored("router disconnected".to_string()),
            token_usage: token_usage(),
            tool_call_count: 2,
            duration_ms: 2_345,
            returned_null: true,
        }),
        WorkflowEvent::Log(WorkflowLogEvent {
            run_id: "run-7".to_string(),
            message: "review complete".to_string(),
        }),
        WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: "run-7".to_string(),
            status: AgentStatus::Completed(Some("ready".to_string())),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 2_048,
            total: Some(8_192),
        }),
    ];

    let actual = events
        .into_iter()
        .map(|event| {
            serde_json::to_value(workflow_event_to_server_notification(
                "thread-1",
                event,
                OBSERVED_AT,
            ))
            .expect("workflow notification should serialize")
        })
        .collect::<Vec<_>>();

    assert_eq!(
        actual,
        vec![
            json!({"method":"workflow/started","params":{"threadId":"thread-1","runId":"run-7","resumedFromRunId":"run-paused","name":"release-audit","phases":["inventory","review"],"argsDigest":"blake3:args","startedAt":OBSERVED_AT}}),
            json!({"method":"workflow/phase/changed","params":{"threadId":"thread-1","runId":"run-7","phaseIndex":1,"title":"review","status":"active","changedAt":OBSERVED_AT}}),
            json!({"method":"workflow/phase/changed","params":{"threadId":"thread-1","runId":"run-7","phaseIndex":1,"title":"review","status":"completed","changedAt":OBSERVED_AT}}),
            json!({"method":"workflow/group/started","params":{"threadId":"thread-1","runId":"run-7","groupId":2,"parentNodeId":null,"kind":"parallel","itemCount":3,"startedAt":OBSERVED_AT}}),
            json!({"method":"workflow/group/completed","params":{"threadId":"thread-1","runId":"run-7","groupId":2,"kind":"parallel","itemCount":3,"completedAt":OBSERVED_AT}}),
            json!({"method":"workflow/agent/started","params":{"threadId":"thread-1","runId":"run-7","nodeId":3,"attempt":2,"lastAttemptReason":"userRetry","parentNodeId":2,"label":"review-api","phase":"review","model":"gpt-5.4","effort":"high","startedAt":OBSERVED_AT}}),
            json!({"method":"workflow/agent/bound","params":{"threadId":"thread-1","runId":"run-7","nodeId":3,"attempt":2,"childThreadId":"019b0214-7c3f-7d80-bd83-88cb759c24a7","boundAt":OBSERVED_AT}}),
            json!({"method":"workflow/agent/updated","params":{"threadId":"thread-1","runId":"run-7","nodeId":3,"attempt":2,"lastAttemptReason":"userRetry","tokenUsage":{"totalTokens":21,"inputTokens":13,"cachedInputTokens":3,"outputTokens":8,"reasoningOutputTokens":5},"toolCallCount":2,"durationMs":1234,"updatedAt":OBSERVED_AT}}),
            json!({"method":"workflow/agent/completed","params":{"threadId":"thread-1","runId":"run-7","nodeId":3,"attempt":5,"lastAttemptReason":"retryLimitReached","status":CollabAgentStatus::Errored,"message":"router disconnected","tokenUsage":{"totalTokens":21,"inputTokens":13,"cachedInputTokens":3,"outputTokens":8,"reasoningOutputTokens":5},"toolCallCount":2,"durationMs":2345,"returnedNull":true,"completedAt":OBSERVED_AT}}),
            json!({"method":"workflow/log","params":{"threadId":"thread-1","runId":"run-7","message":"review complete","emittedAt":OBSERVED_AT}}),
            json!({"method":"workflow/completed","params":{"threadId":"thread-1","runId":"run-7","status":CollabAgentStatus::Completed,"message":"ready","terminalReason":"completed","spent":2048,"total":8192,"completedAt":OBSERVED_AT}}),
        ]
    );
}

#[test]
fn completed_notification_distinguishes_unmetered_from_zero_limit() {
    let notification = |total| {
        workflow_event_to_server_notification(
            "thread-1",
            WorkflowEvent::RunEnd(WorkflowRunEndEvent {
                run_id: "run-budget".to_string(),
                status: AgentStatus::Completed(None),
                terminal_reason: Some(WorkflowRunTerminalReason::Completed),
                spent: 0,
                total,
            }),
            OBSERVED_AT,
        )
    };
    for (total, expected_wire_total) in [(None, serde_json::Value::Null), (Some(0), json!(0))] {
        let encoded = serde_json::to_value(notification(total)).expect("serialize notification");
        assert_eq!(encoded["params"]["total"], expected_wire_total);
    }
}

#[test]
fn legacy_optional_workflow_fields_serialize_as_null() {
    let started = workflow_event_to_server_notification(
        "thread-1",
        WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: "run-legacy".to_string(),
            resumed_from_run_id: None,
            name: "legacy".to_string(),
            phases: Vec::new(),
            args_digest: "blake3:args".to_string(),
        }),
        OBSERVED_AT,
    );
    let completed = workflow_event_to_server_notification(
        "thread-1",
        WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: "run-legacy".to_string(),
            status: AgentStatus::Interrupted,
            terminal_reason: None,
            spent: 0,
            total: None,
        }),
        OBSERVED_AT,
    );

    assert_eq!(
        serde_json::to_value(started).expect("serialize started notification")["params"]["resumedFromRunId"],
        serde_json::Value::Null
    );
    assert_eq!(
        serde_json::to_value(completed).expect("serialize completed notification")["params"]["terminalReason"],
        serde_json::Value::Null
    );
}

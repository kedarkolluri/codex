use super::*;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use pretty_assertions::assert_eq;

#[test]
fn resume_snapshot_coalesces_agent_updates_and_bounds_logs() {
    let mut items = vec![workflow_item(WorkflowEvent::RunBegin(
        WorkflowRunBeginEvent {
            run_id: "run-1".to_string(),
            resumed_from_run_id: None,
            name: "audit".to_string(),
            phases: Vec::new(),
            args_digest: "blake3:args".to_string(),
        },
    ))];
    items.extend([
        workflow_item(WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: None,
            label: "review".to_string(),
            phase: None,
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffort::High,
        })),
        workflow_item(WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 0,
            child_thread_id: "thread-1".to_string(),
        })),
    ]);
    items.extend((0..=MAX_LOG_EVENTS_PER_RUN).map(|index| {
        workflow_item(WorkflowEvent::Log(WorkflowLogEvent {
            run_id: "run-1".to_string(),
            message: format!("log-{index}"),
        }))
    }));
    items.extend([3, 7].map(|tool_call_count| {
        workflow_item(WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: TokenUsage::default(),
            tool_call_count,
            duration_ms: 0,
        }))
    }));

    let events = workflow_events_for_resume(&items);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, WorkflowEvent::Log(_)))
            .count(),
        MAX_LOG_EVENTS_PER_RUN
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, WorkflowEvent::Log(log) if log.message == "log-0"))
    );
    assert_eq!(
        events.iter().find_map(|event| match event {
            WorkflowEvent::AgentUpdated(event) => Some(event.tool_call_count),
            _ => None,
        }),
        Some(7)
    );
    let lifecycle = events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::AgentBegin(_) => Some("begin"),
            WorkflowEvent::AgentBound(_) => Some("bound"),
            WorkflowEvent::AgentUpdated(_) => Some("updated"),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(lifecycle, vec!["begin", "bound", "updated"]);
}

#[test]
fn completed_attempt_does_not_suppress_running_retry_update() {
    let retry_reason = Some(WorkflowAgentAttemptReason::UserRetry);
    let items = vec![
        run_begin_item("run-1"),
        workflow_item(WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: None,
            label: "review".to_string(),
            phase: None,
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffort::High,
        })),
        workflow_item(WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: TokenUsage::default(),
            tool_call_count: 3,
            duration_ms: 100,
        })),
        workflow_item(WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            status: AgentStatus::Errored("retry requested".to_string()),
            token_usage: TokenUsage::default(),
            tool_call_count: 3,
            duration_ms: 100,
            returned_null: true,
        })),
        workflow_item(WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 1,
            last_attempt_reason: retry_reason,
            parent_node_id: None,
            label: "review".to_string(),
            phase: None,
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffort::High,
        })),
        workflow_item(WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 1,
            last_attempt_reason: retry_reason,
            token_usage: TokenUsage::default(),
            tool_call_count: 5,
            duration_ms: 200,
        })),
        workflow_item(WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: "run-1".to_string(),
            node_id: 1,
            attempt: 1,
            last_attempt_reason: retry_reason,
            token_usage: TokenUsage::default(),
            tool_call_count: 8,
            duration_ms: 300,
        })),
    ];

    let events = workflow_events_for_resume(&items);
    let updates = events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::AgentUpdated(update) => Some((
                update.node_id,
                update.attempt,
                update.tool_call_count,
                update.duration_ms,
            )),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(updates, vec![(1, 1, 8, 300)]);
}

#[test]
fn latest_outer_run_keeps_its_begin_after_many_nested_runs() {
    let mut items = vec![run_begin_item("outer")];
    for index in 0..MAX_REPLAY_RUNS {
        let run_id = format!("child-{index}");
        items.push(run_begin_item(&run_id));
        items.push(run_end_item(&run_id));
    }
    items.push(run_end_item("outer"));

    let events = workflow_events_for_resume(&items);
    let outer = events
        .iter()
        .filter(|event| workflow_run_id(event) == "outer")
        .collect::<Vec<_>>();
    assert_eq!(outer.len(), 2);
    assert!(matches!(outer[0], WorkflowEvent::RunBegin(_)));
    assert!(matches!(outer[1], WorkflowEvent::RunEnd(_)));
}

#[test]
fn active_outer_run_is_prioritized_over_newer_completed_runs() {
    let mut items = vec![run_begin_item("outer")];
    for index in 0..MAX_REPLAY_RUNS {
        let run_id = format!("child-{index}");
        items.push(run_begin_item(&run_id));
        items.push(run_end_item(&run_id));
    }

    let events = workflow_events_for_resume(&items);
    let replayed_lifecycle = events
        .iter()
        .map(|event| match event {
            WorkflowEvent::RunBegin(event) => ("begin", event.run_id.as_str()),
            WorkflowEvent::RunEnd(event) => ("end", event.run_id.as_str()),
            event => panic!("unexpected replay event: {event:?}"),
        })
        .collect::<Vec<_>>();

    assert_eq!(
        replayed_lifecycle,
        vec![
            ("begin", "outer"),
            ("begin", "child-1"),
            ("end", "child-1"),
            ("begin", "child-2"),
            ("end", "child-2"),
            ("begin", "child-3"),
            ("end", "child-3"),
        ]
    );
}

fn run_begin_item(run_id: &str) -> RolloutItem {
    workflow_item(WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: run_id.to_string(),
        resumed_from_run_id: None,
        name: run_id.to_string(),
        phases: Vec::new(),
        args_digest: "digest".to_string(),
    }))
}

fn run_end_item(run_id: &str) -> RolloutItem {
    workflow_item(WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: run_id.to_string(),
        status: AgentStatus::Completed(None),
        terminal_reason: Some(WorkflowRunTerminalReason::Completed),
        spent: 0,
        total: Some(0),
    }))
}

fn workflow_item(event: WorkflowEvent) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::Workflow(event))
}

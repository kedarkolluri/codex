use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use pretty_assertions::assert_eq;

use super::WorkflowModelError;
use super::WorkflowRunModel;

const RUN_ID: &str = "run-validation";

#[test]
fn terminal_events_reject_running_status_transactionally() {
    let mut model = started_agent();
    let before = model.clone();
    let event = WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
        run_id: RUN_ID.to_string(),
        node_id: 0,
        attempt: 0,
        last_attempt_reason: None,
        status: AgentStatus::Running,
        token_usage: TokenUsage::default(),
        tool_call_count: 0,
        duration_ms: 0,
        returned_null: false,
    });

    assert_eq!(
        model.apply(&event),
        Err(WorkflowModelError::NonTerminalStatus {
            node_id: Some(0),
            status: AgentStatus::Running,
        })
    );
    assert_eq!(model, before);
}

#[test]
fn phase_and_group_completion_require_children_to_be_terminal() {
    let mut model = started_agent();
    let before = model.clone();
    assert_eq!(
        model.apply(&phase_end()),
        Err(WorkflowModelError::ActiveTopologyAtPhaseEnd {
            phase_index: 0,
            node_id: 0,
        })
    );
    assert_eq!(model, before);

    let mut grouped = base_model();
    apply(
        &mut grouped,
        WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: RUN_ID.to_string(),
            group_id: 1,
            parent_node_id: None,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }),
    );
    apply(&mut grouped, agent_begin(2, Some(1)));
    let before = grouped.clone();
    let end = WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
        run_id: RUN_ID.to_string(),
        group_id: 1,
        kind: WorkflowGroupKind::Parallel,
        item_count: 1,
    });
    assert_eq!(
        grouped.apply(&end),
        Err(WorkflowModelError::ActiveChildAtGroupEnd {
            group_id: 1,
            node_id: 2,
        })
    );
    assert_eq!(grouped, before);
}

#[test]
fn agent_counters_cannot_move_backwards() {
    let mut model = started_agent();
    apply(
        &mut model,
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: RUN_ID.to_string(),
            node_id: 0,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: usage(10),
            tool_call_count: 2,
            duration_ms: 100,
        }),
    );
    let before = model.clone();
    let regression = WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
        run_id: RUN_ID.to_string(),
        node_id: 0,
        attempt: 0,
        last_attempt_reason: None,
        token_usage: usage(9),
        tool_call_count: 2,
        duration_ms: 100,
    });
    assert!(matches!(
        model.apply(&regression),
        Err(WorkflowModelError::CounterRegression {
            node_id: 0,
            field: "input_tokens",
            ..
        })
    ));
    assert_eq!(model, before);
}

#[test]
fn agent_binding_requires_a_known_unbound_agent_transactionally() {
    let mut model = base_model();
    let before_unknown = model.clone();
    assert_eq!(
        model.apply(&agent_bound(99, "thread-99")),
        Err(WorkflowModelError::UnknownAgent { node_id: 99 })
    );
    assert_eq!(model, before_unknown);

    apply(&mut model, agent_begin(0, None));
    apply(&mut model, agent_bound(0, "thread-0"));
    let before_duplicate = model.clone();
    assert_eq!(model.apply(&agent_bound(0, "thread-0")), Ok(()));
    assert_eq!(model, before_duplicate);

    let before_conflict = model.clone();
    assert_eq!(
        model.apply(&agent_bound(0, "another-thread")),
        Err(WorkflowModelError::ConflictingAgentBinding {
            node_id: 0,
            existing_child_thread_id: "thread-0".to_string(),
            child_thread_id: "another-thread".to_string(),
        })
    );
    assert_eq!(model, before_conflict);
}

#[test]
fn agent_updates_and_completion_require_binding_transactionally() {
    let mut model = base_model();
    apply(&mut model, agent_begin(0, None));
    let before = model.clone();

    let update = WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
        run_id: RUN_ID.to_string(),
        node_id: 0,
        attempt: 0,
        last_attempt_reason: None,
        token_usage: TokenUsage::default(),
        tool_call_count: 0,
        duration_ms: 0,
    });
    assert_eq!(
        model.apply(&update),
        Err(WorkflowModelError::AgentNotBound { node_id: 0 })
    );
    assert_eq!(model, before);

    let end = WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
        run_id: RUN_ID.to_string(),
        node_id: 0,
        attempt: 0,
        last_attempt_reason: None,
        status: AgentStatus::Completed(None),
        token_usage: TokenUsage::default(),
        tool_call_count: 0,
        duration_ms: 0,
        returned_null: false,
    });
    assert_eq!(
        model.apply(&end),
        Err(WorkflowModelError::AgentNotBound { node_id: 0 })
    );
    assert_eq!(model, before);
}

#[test]
fn run_end_rejects_nonterminal_status_after_topology_completes() {
    let mut model = started_agent();
    apply(
        &mut model,
        WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id: RUN_ID.to_string(),
            node_id: 0,
            attempt: 0,
            last_attempt_reason: None,
            status: AgentStatus::Completed(None),
            token_usage: TokenUsage::default(),
            tool_call_count: 0,
            duration_ms: 0,
            returned_null: false,
        }),
    );
    apply(&mut model, phase_end());
    let before = model.clone();
    let event = WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: RUN_ID.to_string(),
        status: AgentStatus::PendingInit,
        terminal_reason: None,
        spent: 0,
        total: Some(0),
    });
    assert_eq!(
        model.apply(&event),
        Err(WorkflowModelError::NonTerminalStatus {
            node_id: None,
            status: AgentStatus::PendingInit,
        })
    );
    assert_eq!(model, before);
}

fn started_agent() -> WorkflowRunModel {
    let mut model = base_model();
    apply(&mut model, agent_begin(0, None));
    apply(&mut model, agent_bound(0, "thread-0"));
    model
}

fn base_model() -> WorkflowRunModel {
    let mut model = WorkflowRunModel::from_event(&WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "validation".to_string(),
        phases: vec!["work".to_string()],
        args_digest: "digest".to_string(),
    }))
    .expect("run begin should project");
    apply(
        &mut model,
        WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: RUN_ID.to_string(),
            phase_index: 0,
            title: "work".to_string(),
        }),
    );
    model
}

fn agent_begin(node_id: u64, parent_node_id: Option<u64>) -> WorkflowEvent {
    WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        parent_node_id,
        label: format!("agent-{node_id}"),
        phase: Some("work".to_string()),
        model: "gpt-5.4".to_string(),
        effort: ReasoningEffort::High,
    })
}

fn agent_bound(node_id: u64, child_thread_id: &str) -> WorkflowEvent {
    WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        child_thread_id: child_thread_id.to_string(),
    })
}

fn phase_end() -> WorkflowEvent {
    WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
        run_id: RUN_ID.to_string(),
        phase_index: 0,
        title: "work".to_string(),
    })
}

fn usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens,
        cached_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
        total_tokens,
    }
}

fn apply(model: &mut WorkflowRunModel, event: WorkflowEvent) {
    model.apply(&event).expect("event should project");
}

use std::collections::BTreeMap;

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

use super::*;

#[path = "run_model_test_support.rs"]
mod support;
use support::expected_completed_model;

const RUN_ID: &str = "run-7";

fn run_begin(phases: &[&str]) -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "release-audit".to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        args_digest: "blake3:args".to_string(),
    })
}

fn phase_begin(phase_index: u64, title: &str) -> WorkflowEvent {
    WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
        run_id: RUN_ID.to_string(),
        phase_index,
        title: title.to_string(),
    })
}

fn phase_end(phase_index: u64, title: &str) -> WorkflowEvent {
    WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
        run_id: RUN_ID.to_string(),
        phase_index,
        title: title.to_string(),
    })
}

fn group_begin(
    group_id: u64,
    parent_node_id: Option<u64>,
    kind: WorkflowGroupKind,
    item_count: u64,
) -> WorkflowEvent {
    WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        parent_node_id,
        kind,
        item_count,
    })
}

fn group_end(group_id: u64, kind: WorkflowGroupKind, item_count: u64) -> WorkflowEvent {
    WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        kind,
        item_count,
    })
}

fn agent_begin(
    node_id: u64,
    parent_node_id: Option<u64>,
    label: &str,
    phase: Option<&str>,
) -> WorkflowEvent {
    WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        parent_node_id,
        label: label.to_string(),
        phase: phase.map(ToString::to_string),
        model: "gpt-5.4".to_string(),
        effort: ReasoningEffort::High,
    })
}

fn agent_bound(node_id: u64) -> WorkflowEvent {
    WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        child_thread_id: format!("thread-{node_id}"),
    })
}

fn token_usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens - 5,
        cached_input_tokens: 2,
        output_tokens: 5,
        reasoning_output_tokens: 3,
        total_tokens,
    }
}

fn agent_updated(node_id: u64, total_tokens: i64, tool_call_count: u64) -> WorkflowEvent {
    WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        token_usage: token_usage(total_tokens),
        tool_call_count,
        duration_ms: 0,
    })
}

fn agent_end(
    node_id: u64,
    status: AgentStatus,
    total_tokens: i64,
    tool_call_count: u64,
    returned_null: bool,
) -> WorkflowEvent {
    WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        status,
        token_usage: token_usage(total_tokens),
        tool_call_count,
        duration_ms: 0,
        returned_null,
    })
}

fn run_end() -> WorkflowEvent {
    WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: RUN_ID.to_string(),
        status: AgentStatus::Completed(Some("done".to_string())),
        terminal_reason: Some(WorkflowRunTerminalReason::Completed),
        spent: 25,
        total: Some(1_000),
    })
}

fn project(events: &[WorkflowEvent]) -> Result<WorkflowRunModel, WorkflowModelError> {
    let Some((begin, remaining)) = events.split_first() else {
        return Err(WorkflowModelError::ExpectedRunBegin);
    };
    let mut model = WorkflowRunModel::from_event(begin)?;
    for event in remaining {
        model.apply(event)?;
    }
    Ok(model)
}

#[test]
fn run_begin_seeds_every_declared_phase_pending() {
    let model = WorkflowRunModel::from_event(&run_begin(&["plan", "execute", "verify"]))
        .expect("run begin should create a model");

    assert_eq!(
        model,
        WorkflowRunModel {
            run_id: RUN_ID.to_string(),
            resumed_from_run_id: None,
            name: "release-audit".to_string(),
            args_digest: "blake3:args".to_string(),
            state: WorkflowRunState::Running,
            status: AgentStatus::Running,
            terminal_reason: None,
            phases: vec![
                phase(0, "plan", WorkflowPhaseState::Pending, false),
                phase(1, "execute", WorkflowPhaseState::Pending, false),
                phase(2, "verify", WorkflowPhaseState::Pending, false),
            ],
            topology: BTreeMap::new(),
            topology_order: Vec::new(),
            aggregate: WorkflowAggregate::default(),
            budget: None,
            active_phase_index: None,
            next_phase_index: 0,
        }
    );
}

#[test]
fn terminal_projection_distinguishes_unmetered_from_zero_limit() {
    for total in [None, Some(0)] {
        let mut terminal = run_end();
        let WorkflowEvent::RunEnd(event) = &mut terminal else {
            unreachable!("run_end helper returns a terminal event")
        };
        event.spent = 0;
        event.total = total;
        let model = project(&[run_begin(&[]), terminal]).expect("terminal projection");
        assert_eq!(
            model.budget,
            Some(WorkflowBudgetSummary { spent: 0, total })
        );
    }
}

#[test]
fn projection_preserves_nested_and_empty_topology_and_rolls_up_counters() {
    let events = vec![
        run_begin(&["plan", "execute"]),
        phase_begin(0, "plan"),
        group_begin(10, None, WorkflowGroupKind::Parallel, 2),
        group_begin(11, Some(10), WorkflowGroupKind::Pipeline, 0),
        group_end(11, WorkflowGroupKind::Pipeline, 0),
        agent_begin(12, Some(10), "review-api", Some("plan")),
        agent_bound(12),
        agent_updated(12, 10, 1),
        agent_end(
            12,
            AgentStatus::Completed(Some("ok".to_string())),
            20,
            2,
            false,
        ),
        group_end(10, WorkflowGroupKind::Parallel, 2),
        phase_end(0, "plan"),
        phase_begin(1, "execute"),
        agent_begin(13, None, "run-tests", Some("execute")),
        agent_bound(13),
        agent_end(13, AgentStatus::Errored("boom".to_string()), 5, 1, true),
        phase_end(1, "execute"),
        phase_begin(2, "verify"),
        group_begin(14, None, WorkflowGroupKind::Parallel, 0),
        group_end(14, WorkflowGroupKind::Parallel, 0),
        phase_end(2, "verify"),
        run_end(),
    ];

    let updated = project(&events[..=7]).expect("live counter update should project");
    assert_eq!(
        updated.aggregate,
        WorkflowAggregate {
            group_count: 2,
            agent_count: 1,
            active_agent_count: 1,
            token_usage: token_usage(10),
            tool_call_count: 1,
            ..WorkflowAggregate::default()
        }
    );
    assert_eq!(
        updated.topology.get(&12),
        Some(&WorkflowTopologyNode::Agent(WorkflowAgent {
            id: 12,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: Some(10),
            phase_index: 0,
            label: "review-api".to_string(),
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffort::High,
            child_thread_id: Some("thread-12".to_string()),
            state: WorkflowNodeState::Active,
            status: AgentStatus::Running,
            token_usage: token_usage(10),
            tool_call_count: 1,
            duration_ms: 0,
            returned_null: false,
            child_node_ids: Vec::new(),
        }))
    );

    let first = project(&events).expect("valid event sequence should project");
    let second = project(&events).expect("same event sequence should project again");
    assert_eq!(first, second);
    assert_eq!(first, expected_completed_model());
}

#[test]
fn retried_live_projection_matches_synthetic_replay_projection() {
    let begin_initial = agent_begin(12, None, "review-api", Some("plan"));
    let mut begin_retry = begin_initial.clone();
    let WorkflowEvent::AgentBegin(begin_retry_event) = &mut begin_retry else {
        unreachable!("agent_begin helper returns AgentBegin")
    };
    begin_retry_event.attempt = 1;
    begin_retry_event.last_attempt_reason = Some(WorkflowAgentAttemptReason::UserRetry);
    let bound_initial = agent_bound(12);
    let mut bound_retry = bound_initial.clone();
    let WorkflowEvent::AgentBound(bound_retry_event) = &mut bound_retry else {
        unreachable!("agent_bound helper returns AgentBound")
    };
    bound_retry_event.attempt = 1;
    bound_retry_event.child_thread_id = "thread-12-retry".to_string();
    let first_progress = WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
        run_id: RUN_ID.to_string(),
        node_id: 12,
        attempt: 0,
        last_attempt_reason: None,
        token_usage: token_usage(10),
        tool_call_count: 1,
        duration_ms: 100,
    });
    let aggregate_progress = WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
        run_id: RUN_ID.to_string(),
        node_id: 12,
        attempt: 1,
        last_attempt_reason: Some(WorkflowAgentAttemptReason::UserRetry),
        token_usage: token_usage(25),
        tool_call_count: 3,
        duration_ms: 250,
    });
    let terminal = WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
        run_id: RUN_ID.to_string(),
        node_id: 12,
        attempt: 1,
        last_attempt_reason: Some(WorkflowAgentAttemptReason::UserRetry),
        status: AgentStatus::Completed(Some("ok".to_string())),
        token_usage: token_usage(25),
        tool_call_count: 3,
        duration_ms: 250,
        returned_null: false,
    });
    let common_tail = [phase_end(0, "plan"), run_end()];

    let mut live = vec![
        run_begin(&["plan"]),
        phase_begin(0, "plan"),
        begin_initial.clone(),
        bound_initial,
        first_progress,
        begin_retry.clone(),
        bound_retry.clone(),
        aggregate_progress.clone(),
        terminal.clone(),
    ];
    live.extend(common_tail.clone());

    // A final journal anchor at attempt 1 reconstructs the strict generation path without
    // fabricating an intermediate binding/update. Aggregate counters come from the anchor.
    let mut replay = vec![
        run_begin(&["plan"]),
        phase_begin(0, "plan"),
        begin_initial,
        begin_retry,
        bound_retry,
        aggregate_progress,
        terminal,
    ];
    replay.extend(common_tail);

    let live_model = project(&live).expect("live retry projection");
    let replay_model = project(&replay).expect("synthetic retry replay projection");
    assert_eq!(replay_model, live_model);
    let WorkflowTopologyNode::Agent(agent) = &live_model.topology[&12] else {
        panic!("node 12 must remain an agent")
    };
    assert_eq!(agent.attempt, 1);
    assert_eq!(
        agent.last_attempt_reason,
        Some(WorkflowAgentAttemptReason::UserRetry)
    );
    assert_eq!(agent.token_usage, token_usage(25));
    assert_eq!(agent.tool_call_count, 3);
    assert_eq!(agent.duration_ms, 250);
}

#[test]
fn empty_phase_list_uses_one_implicit_root_phase() {
    let events = vec![
        run_begin(&[]),
        group_begin(42, None, WorkflowGroupKind::Parallel, 0),
        group_end(42, WorkflowGroupKind::Parallel, 0),
        run_end(),
    ];

    let model = project(&events).expect("phase-less run should project");

    assert_eq!(
        model.phases,
        vec![WorkflowPhase {
            index: 0,
            title: "root".to_string(),
            state: WorkflowPhaseState::Completed,
            implicit: true,
            root_node_ids: vec![42],
            aggregate: WorkflowAggregate {
                group_count: 1,
                ..WorkflowAggregate::default()
            },
        }]
    );
    assert_eq!(
        model.topology,
        BTreeMap::from([(
            42,
            WorkflowTopologyNode::Group(WorkflowGroup {
                id: 42,
                parent_node_id: None,
                phase_index: 0,
                kind: WorkflowGroupKind::Parallel,
                item_count: 0,
                state: WorkflowNodeState::Completed,
                child_node_ids: Vec::new(),
            })
        )])
    );
}

#[test]
fn first_dynamic_phase_replaces_a_populated_implicit_root() {
    let events = vec![
        run_begin(&[]),
        agent_begin(5, None, "scan", None),
        agent_bound(5),
        agent_end(5, AgentStatus::Completed(None), 8, 0, false),
        phase_begin(0, "discover"),
        phase_end(0, "discover"),
        phase_begin(1, "summarize"),
    ];

    let model = project(&events).expect("dynamic phases should append in event order");

    assert_eq!(
        model.phases,
        vec![
            WorkflowPhase {
                index: 0,
                title: "discover".to_string(),
                state: WorkflowPhaseState::Completed,
                implicit: false,
                root_node_ids: vec![5],
                aggregate: WorkflowAggregate {
                    agent_count: 1,
                    completed_agent_count: 1,
                    token_usage: token_usage(8),
                    ..WorkflowAggregate::default()
                },
            },
            WorkflowPhase {
                index: 1,
                title: "summarize".to_string(),
                state: WorkflowPhaseState::Active,
                implicit: false,
                root_node_ids: Vec::new(),
                aggregate: WorkflowAggregate::default(),
            },
        ]
    );
}

#[test]
fn explicit_group_parents_preserve_nested_and_overlapping_lifetimes() {
    let events = vec![
        run_begin(&["plan"]),
        phase_begin(0, "plan"),
        group_begin(1, None, WorkflowGroupKind::Parallel, 0),
        group_begin(2, None, WorkflowGroupKind::Pipeline, 1),
        group_end(1, WorkflowGroupKind::Parallel, 0),
        group_begin(3, Some(2), WorkflowGroupKind::Parallel, 0),
        group_end(3, WorkflowGroupKind::Parallel, 0),
        group_end(2, WorkflowGroupKind::Pipeline, 1),
        phase_end(0, "plan"),
        run_end(),
    ];

    let model = project(&events).expect("overlapping group lifetimes should project by ID");

    assert_eq!(
        model.topology,
        BTreeMap::from([
            (
                1,
                WorkflowTopologyNode::Group(WorkflowGroup {
                    id: 1,
                    parent_node_id: None,
                    phase_index: 0,
                    kind: WorkflowGroupKind::Parallel,
                    item_count: 0,
                    state: WorkflowNodeState::Completed,
                    child_node_ids: Vec::new(),
                }),
            ),
            (
                2,
                WorkflowTopologyNode::Group(WorkflowGroup {
                    id: 2,
                    parent_node_id: None,
                    phase_index: 0,
                    kind: WorkflowGroupKind::Pipeline,
                    item_count: 1,
                    state: WorkflowNodeState::Completed,
                    child_node_ids: vec![3],
                }),
            ),
            (
                3,
                WorkflowTopologyNode::Group(WorkflowGroup {
                    id: 3,
                    parent_node_id: Some(2),
                    phase_index: 0,
                    kind: WorkflowGroupKind::Parallel,
                    item_count: 0,
                    state: WorkflowNodeState::Completed,
                    child_node_ids: Vec::new(),
                }),
            ),
        ])
    );
    assert_eq!(model.phases[0].root_node_ids, vec![1, 2]);
}

#[test]
fn malformed_topology_events_are_reported_without_mutating_the_model() {
    let mut model = project(&[
        run_begin(&["plan"]),
        phase_begin(0, "plan"),
        group_begin(7, None, WorkflowGroupKind::Parallel, 1),
    ])
    .expect("setup should project");
    let before = model.clone();

    let duplicate = agent_begin(7, Some(7), "duplicate", Some("plan"));
    assert_eq!(
        model.apply(&duplicate),
        Err(WorkflowModelError::DuplicateTopologyId { node_id: 7 })
    );
    assert_eq!(model, before);

    let missing_parent = agent_begin(8, Some(99), "orphan", Some("plan"));
    assert_eq!(
        model.apply(&missing_parent),
        Err(WorkflowModelError::MissingParent {
            node_id: 8,
            parent_node_id: 99,
        })
    );
    assert_eq!(model, before);

    let missing_group_parent = group_begin(8, Some(99), WorkflowGroupKind::Pipeline, 0);
    assert_eq!(
        model.apply(&missing_group_parent),
        Err(WorkflowModelError::MissingParent {
            node_id: 8,
            parent_node_id: 99,
        })
    );
    assert_eq!(model, before);
}

#[test]
fn run_and_phase_mismatches_are_reported_without_mutation() {
    let mut model = WorkflowRunModel::from_event(&run_begin(&["plan", "execute"]))
        .expect("run begin should create a model");
    let before = model.clone();

    assert_eq!(
        model.apply(&phase_begin(1, "execute")),
        Err(WorkflowModelError::UnexpectedPhaseIndex {
            expected: 0,
            actual: 1,
        })
    );
    assert_eq!(model, before);

    let wrong_run = WorkflowEvent::Log(WorkflowLogEvent {
        run_id: "another-run".to_string(),
        message: "ignored".to_string(),
    });
    assert_eq!(
        model.apply(&wrong_run),
        Err(WorkflowModelError::RunIdMismatch {
            expected: RUN_ID.to_string(),
            actual: "another-run".to_string(),
        })
    );
    assert_eq!(model, before);
}

fn phase(index: u64, title: &str, state: WorkflowPhaseState, implicit: bool) -> WorkflowPhase {
    WorkflowPhase {
        index,
        title: title.to_string(),
        state,
        implicit,
        root_node_ids: Vec::new(),
        aggregate: WorkflowAggregate::default(),
    }
}

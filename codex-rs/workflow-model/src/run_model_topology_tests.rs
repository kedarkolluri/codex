use std::collections::BTreeMap;

use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use pretty_assertions::assert_eq;

use super::*;

const RUN_ID: &str = "run-topology";

macro_rules! begin_group {
    ($id:expr, parent: $parent:expr, $kind:ident, items: $items:expr) => {
        WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: RUN_ID.to_string(),
            group_id: $id,
            parent_node_id: $parent,
            kind: WorkflowGroupKind::$kind,
            item_count: $items,
        })
    };
}

macro_rules! end_group {
    ($id:expr, $kind:ident, items: $items:expr) => {
        WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
            run_id: RUN_ID.to_string(),
            group_id: $id,
            kind: WorkflowGroupKind::$kind,
            item_count: $items,
        })
    };
}

#[test]
fn nested_overlapping_and_empty_groups_reduce_deterministically() {
    let events = vec![
        run_begin(&["plan"]),
        phase_begin(/*phase_index*/ 0, "plan"),
        begin_group!(0, parent: None, Parallel, items: 2),
        begin_group!(1, parent: Some(0), Pipeline, items: 0),
        end_group!(1, Pipeline, items: 0),
        begin_group!(2, parent: Some(0), Pipeline, items: 0),
        end_group!(2, Pipeline, items: 0),
        begin_group!(3, parent: None, Pipeline, items: 1),
        begin_group!(4, parent: Some(3), Parallel, items: 0),
        end_group!(0, Parallel, items: 2),
        end_group!(4, Parallel, items: 0),
        end_group!(3, Pipeline, items: 1),
        phase_end(/*phase_index*/ 0, "plan"),
    ];

    let first = project(&events).expect("valid group topology should project");
    let second = project(&events).expect("replayed group topology should project");
    assert_eq!(first, second);
    assert_eq!(first.phases[0].root_node_ids, vec![0, 3]);
    assert_eq!(first.phases[0].state, WorkflowPhaseState::Completed);
    use WorkflowGroupKind::Parallel;
    use WorkflowGroupKind::Pipeline;
    let id_0 = 0;
    let id_1 = 1;
    let id_2 = 2;
    let id_3 = 3;
    let id_4 = 4;
    let root = None;
    let zero = 0;
    let one = 1;
    let two = 2;
    assert_eq!(
        first.topology,
        BTreeMap::from([
            (0, group(id_0, root, Parallel, two, vec![1, 2])),
            (1, group(id_1, Some(0), Pipeline, zero, Vec::new())),
            (2, group(id_2, Some(0), Pipeline, zero, Vec::new())),
            (3, group(id_3, root, Pipeline, one, vec![4])),
            (4, group(id_4, Some(3), Parallel, zero, Vec::new())),
        ])
    );
    assert_eq!(first.next_topology_id, 5);
}

#[test]
fn malformed_group_topology_is_transactional() {
    let mut without_phase =
        WorkflowRunModel::from_event(&run_begin(&["plan"])).expect("run should begin");
    assert_rejected_unchanged(
        &mut without_phase,
        &begin_group!(0, parent: None, Parallel, items: 0),
        WorkflowModelError::NoActivePhase,
    );

    let base = project(&[
        run_begin(&["plan"]),
        phase_begin(/*phase_index*/ 0, "plan"),
        begin_group!(0, parent: None, Parallel, items: 1),
    ])
    .expect("base topology should project");
    for (event, expected) in [
        (
            begin_group!(0, parent: None, Parallel, items: 1),
            WorkflowModelError::DuplicateTopologyId { node_id: 0 },
        ),
        (
            begin_group!(2, parent: None, Parallel, items: 0),
            WorkflowModelError::UnexpectedTopologyId {
                expected: 1,
                actual: 2,
            },
        ),
        (
            begin_group!(1, parent: Some(99), Pipeline, items: 0),
            WorkflowModelError::MissingParent {
                node_id: 1,
                parent_node_id: 99,
            },
        ),
        (
            end_group!(99, Parallel, items: 0),
            WorkflowModelError::UnknownGroup { group_id: 99 },
        ),
        (
            end_group!(0, Pipeline, items: 1),
            WorkflowModelError::GroupDefinitionMismatch { group_id: 0 },
        ),
    ] {
        let mut model = base.clone();
        assert_rejected_unchanged(&mut model, &event, expected);
    }

    let mut nested = base;
    apply(
        &mut nested,
        &begin_group!(1, parent: Some(0), Pipeline, items: 0),
    );
    assert_rejected_unchanged(
        &mut nested,
        &end_group!(0, Parallel, items: 1),
        WorkflowModelError::ActiveChildAtGroupEnd {
            group_id: 0,
            node_id: 1,
        },
    );
    apply(&mut nested, &end_group!(1, Pipeline, items: 0));
    apply(&mut nested, &end_group!(0, Parallel, items: 1));
    assert_rejected_unchanged(
        &mut nested,
        &begin_group!(2, parent: Some(0), Parallel, items: 0),
        WorkflowModelError::ParentNotActive {
            node_id: 2,
            parent_node_id: 0,
        },
    );
    assert_rejected_unchanged(
        &mut nested,
        &end_group!(0, Parallel, items: 1),
        WorkflowModelError::NodeAlreadyCompleted { node_id: 0 },
    );
}

#[test]
fn phase_boundaries_wait_for_active_topology() {
    let mut implicit = WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin");
    apply(
        &mut implicit,
        &begin_group!(0, parent: None, Parallel, items: 0),
    );
    assert_rejected_unchanged(
        &mut implicit,
        &phase_begin(/*phase_index*/ 0, "discover"),
        WorkflowModelError::ActiveTopologyAtPhaseBoundary {
            phase_index: 0,
            node_id: 0,
        },
    );
    apply(&mut implicit, &end_group!(0, Parallel, items: 0));
    apply(&mut implicit, &phase_begin(/*phase_index*/ 0, "discover"));
    assert_eq!(implicit.phases[0].root_node_ids, vec![0]);
    apply(&mut implicit, &phase_end(/*phase_index*/ 0, "discover"));

    let mut explicit = project(&[
        run_begin(&["plan"]),
        phase_begin(/*phase_index*/ 0, "plan"),
        begin_group!(0, parent: None, Pipeline, items: 0),
    ])
    .expect("active topology should project");
    assert_rejected_unchanged(
        &mut explicit,
        &phase_end(/*phase_index*/ 0, "plan"),
        WorkflowModelError::ActiveTopologyAtPhaseBoundary {
            phase_index: 0,
            node_id: 0,
        },
    );
    apply(&mut explicit, &end_group!(0, Pipeline, items: 0));
    apply(&mut explicit, &phase_end(/*phase_index*/ 0, "plan"));
    apply(&mut explicit, &phase_begin(/*phase_index*/ 1, "build"));
    apply(
        &mut explicit,
        &begin_group!(1, parent: None, Parallel, items: 0),
    );
    apply(&mut explicit, &end_group!(1, Parallel, items: 0));
    assert_eq!(
        (
            explicit
                .phases
                .iter()
                .map(|phase| phase.root_node_ids.clone())
                .collect::<Vec<_>>(),
            explicit
                .topology
                .values()
                .map(WorkflowTopologyNode::phase_index)
                .collect::<Vec<_>>(),
            explicit.next_topology_id,
        ),
        (vec![vec![0], vec![1]], vec![0, 1], 2)
    );
}

#[test]
fn topology_cap_is_exact_and_transactional() {
    let mut model = WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin");
    for group_id in 0..WORKFLOW_TOPOLOGY_MAX_NODES {
        apply(
            &mut model,
            &begin_group!(group_id, parent: None, Parallel, items: 0),
        );
        apply(&mut model, &end_group!(group_id, Parallel, items: 0));
    }
    assert_eq!(model.topology.len(), 4_000);
    assert_eq!(model.phases[0].root_node_ids.len(), 4_000);
    assert_rejected_unchanged(
        &mut model,
        &begin_group!(WORKFLOW_TOPOLOGY_MAX_NODES, parent: None, Parallel, items: 0),
        WorkflowModelError::TopologyLimitExceeded {
            maximum: WORKFLOW_TOPOLOGY_MAX_NODES,
        },
    );
}

#[test]
fn deferred_agent_terminal_events_are_unhandled_without_mutation() {
    let mut model = WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin");
    let events = [
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: RUN_ID.to_string(),
            node_id: 0,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: TokenUsage::default(),
            tool_call_count: 0,
            duration_ms: 0,
        }),
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
    ];
    for event in events {
        let before = model.clone();
        assert_eq!(
            model.reduce_event(&event),
            Ok(ReductionDisposition::Unhandled)
        );
        assert_eq!(model, before);
    }
}

fn run_begin(phases: &[&str]) -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "topology-audit".to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        args_digest: "blake3:topology".to_string(),
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

fn group(
    id: u64,
    parent_node_id: Option<u64>,
    kind: WorkflowGroupKind,
    item_count: u64,
    child_node_ids: Vec<u64>,
) -> WorkflowTopologyNode {
    WorkflowTopologyNode::Group(WorkflowGroup {
        id,
        parent_node_id,
        phase_index: 0,
        kind,
        item_count,
        state: WorkflowNodeState::Completed,
        child_node_ids,
    })
}

fn project(events: &[WorkflowEvent]) -> Result<WorkflowRunModel, WorkflowModelError> {
    let mut events = events.iter();
    let first = events.next().expect("projection needs run_begin");
    let mut model = WorkflowRunModel::from_event(first)?;
    for event in events {
        assert_eq!(model.reduce_event(event)?, ReductionDisposition::Applied);
    }
    Ok(model)
}

fn apply(model: &mut WorkflowRunModel, event: &WorkflowEvent) {
    assert_eq!(model.reduce_event(event), Ok(ReductionDisposition::Applied));
}

fn assert_rejected_unchanged(
    model: &mut WorkflowRunModel,
    event: &WorkflowEvent,
    expected: WorkflowModelError,
) {
    let before = model.clone();
    assert_eq!(model.reduce_event(event), Err(expected));
    assert_eq!(*model, before);
}

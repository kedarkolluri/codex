use std::convert::identity;

use codex_code_mode_protocol::WORKFLOW_AGENT_LABEL_MAX_BYTES as LABEL_MAX;
use codex_code_mode_protocol::WORKFLOW_AGENT_MAX_RETRIES;
use codex_code_mode_protocol::WORKFLOW_AGENT_OPTION_MAX_BYTES as OPTION_MAX;
use codex_code_mode_protocol::WORKFLOW_PHASE_TITLE_MAX_BYTES as PHASE_MAX;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use pretty_assertions::assert_eq;

use super::*;

type Error = WorkflowModelError;
const RUN_ID: &str = "run-agents";
const THREAD_A: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a7";
const THREAD_B: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a8";

macro_rules! assert_rejected {
    ($model:expr, $event:expr, $error:expr) => {
        assert_rejected_unchanged(&mut $model, &$event, $error)
    };
}

macro_rules! assert_agent_text_bounds {
    ($field:ident, $field_name:literal, $maximum:expr, $wrap:expr) => {{
        let mut model = started_phase();
        for value in [" \n ".to_string(), "x".repeat($maximum + 1)] {
            let expected = if value.len() > $maximum {
                Error::TextTooLong {
                    field: $field_name,
                    maximum_bytes: $maximum,
                    actual_bytes: value.len(),
                }
            } else {
                Error::EmptyText { field: $field_name }
            };
            let mut event = agent_begin(/*node_id*/ 0, /*parent_node_id*/ None);
            agent_event_mut(&mut event).$field = ($wrap)(value);
            assert_rejected!(model, event, expected);
        }
    }};
}

#[test]
fn mixed_agent_group_topology_preserves_shared_order_and_parents() {
    let events = vec![
        run_begin(),
        phase_begin(),
        group_begin(/*group_id*/ 0, /*parent_node_id*/ None),
        agent_begin(/*node_id*/ 1, /*parent_node_id*/ Some(0)),
        agent_begin(/*node_id*/ 2, /*parent_node_id*/ Some(1)),
        group_begin(/*group_id*/ 3, /*parent_node_id*/ Some(2)),
    ];
    let first = project(&events);
    assert_eq!(first.phases[0].root_node_ids, vec![0]);
    assert_eq!(first.next_topology_id, 4);
    assert_eq!(
        first
            .topology
            .values()
            .map(WorkflowTopologyNode::parent_node_id)
            .collect::<Vec<_>>(),
        vec![None, Some(0), Some(1), Some(2)]
    );
    assert_eq!(first.topology[&0].child_node_ids(), &[1]);
    assert_eq!(first.topology[&1].child_node_ids(), &[2]);
    assert_eq!(first.topology[&2].child_node_ids(), &[3]);
    assert!(first.topology.values().all(|node| node.phase_index() == 0));
    let mut blocked_group = first;
    assert_rejected!(
        blocked_group,
        group_end(/*group_id*/ 0),
        Error::ActiveChildAtGroupEnd {
            group_id: 0,
            node_id: 1,
        }
    );
    let mut agent_root = project(&[
        run_begin(),
        phase_begin(),
        agent_begin(/*node_id*/ 0, /*parent_node_id*/ None),
    ]);
    assert_rejected!(
        agent_root,
        phase_end(),
        Error::ActiveTopologyAtPhaseBoundary {
            phase_index: 0,
            node_id: 0,
        }
    );
}

#[test]
fn live_retry_and_compact_replay_are_deeply_equal() {
    let initial = agent_begin(/*node_id*/ 0, /*parent_node_id*/ None);
    let mut retry = retry(&initial, /*attempt*/ 1);
    agent_event_mut(&mut retry).phase = Some("plan".to_string());
    let live = project(&[
        run_begin(),
        phase_begin(),
        initial.clone(),
        bound(/*node_id*/ 0, /*attempt*/ 0, THREAD_A),
        retry.clone(),
        bound(/*node_id*/ 0, /*attempt*/ 1, THREAD_B),
    ]);
    let replay = project(&[
        run_begin(),
        phase_begin(),
        initial,
        retry,
        bound(/*node_id*/ 0, /*attempt*/ 1, THREAD_B),
    ]);
    assert_eq!(live, replay);
    assert!(matches!(
        &live.topology[&0],
        WorkflowTopologyNode::Agent(agent)
            if agent.last_attempt_reason() == Some(WorkflowAgentAttemptReason::UserRetry)
    ));
}

#[test]
fn agent_attempts_reasons_and_definitions_are_strict() {
    let initial = agent_begin(/*node_id*/ 0, /*parent_node_id*/ None);
    let mut bad_attempt = initial.clone();
    agent_event_mut(&mut bad_attempt).attempt = 1;
    let mut model = started_phase();
    assert_rejected!(
        model,
        bad_attempt,
        unexpected_attempt(/*node_id*/ 0, /*expected*/ 0, /*actual*/ 1)
    );
    for (attempt, reason) in [
        (0, Some(WorkflowAgentAttemptReason::UserSkip)),
        (0, Some(WorkflowAgentAttemptReason::UserRetry)),
        (0, Some(WorkflowAgentAttemptReason::RetryLimitReached)),
        (1, None),
        (1, Some(WorkflowAgentAttemptReason::UserSkip)),
        (1, Some(WorkflowAgentAttemptReason::RetryLimitReached)),
    ] {
        let mut reason_model = started_phase();
        if attempt == 1 {
            apply(&mut reason_model, &initial);
        }
        let mut event = initial.clone();
        let agent = agent_event_mut(&mut event);
        agent.attempt = attempt;
        agent.last_attempt_reason = reason;
        assert_rejected!(
            reason_model,
            event,
            Error::AgentAttemptReasonMismatch { node_id: 0 }
        );
    }
    apply(&mut model, &initial);
    for actual in [0, 2] {
        assert_rejected!(
            model,
            retry(&initial, actual),
            unexpected_attempt(/*node_id*/ 0, /*expected*/ 1, actual)
        );
    }
    for mutate in [
        |event: &mut WorkflowAgentBeginEvent| event.parent_node_id = Some(99),
        |event: &mut WorkflowAgentBeginEvent| event.label = "changed".to_string(),
        |event: &mut WorkflowAgentBeginEvent| event.model = "changed".to_string(),
        |event: &mut WorkflowAgentBeginEvent| event.effort = ReasoningEffort::Low,
    ] {
        let mut event = retry(&initial, /*attempt*/ 1);
        mutate(agent_event_mut(&mut event));
        assert_rejected!(model, event, Error::AgentDefinitionMismatch { node_id: 0 });
    }
    let mut wrong_phase = retry(&initial, /*attempt*/ 1);
    agent_event_mut(&mut wrong_phase).phase = Some("execute".to_string());
    assert_rejected!(
        model,
        wrong_phase,
        Error::AgentPhaseMismatch {
            node_id: 0,
            expected: "plan".to_string(),
            actual: "execute".to_string(),
        }
    );
    let mut capped = started_phase();
    apply(&mut capped, &initial);
    for attempt in 1..=WORKFLOW_AGENT_MAX_RETRIES {
        apply(&mut capped, &retry(&initial, attempt));
    }
    assert_rejected!(
        capped,
        retry(&initial, WORKFLOW_AGENT_MAX_RETRIES + 1),
        Error::AgentRetryLimitExceeded {
            node_id: 0,
            maximum: WORKFLOW_AGENT_MAX_RETRIES,
        }
    );
}

#[test]
fn agent_text_fields_are_bounded_transactionally() {
    assert_agent_text_bounds!(phase, "agent phase", PHASE_MAX, Some);
    assert_agent_text_bounds!(label, "agent label", LABEL_MAX, identity);
    assert_agent_text_bounds!(model, "agent model", OPTION_MAX, identity);
    assert_agent_text_bounds!(effort, "agent effort", OPTION_MAX, ReasoningEffort::Custom);
}

#[test]
fn agent_binding_is_typed_exact_idempotent_and_unique() {
    let mut model = started_phase();
    let group = group_begin(/*group_id*/ 0, /*parent_node_id*/ None);
    apply(&mut model, &group);
    let first = agent_begin(/*node_id*/ 1, /*parent_node_id*/ Some(0));
    assert_rejected!(
        model,
        agent_begin(/*node_id*/ 0, /*parent_node_id*/ None),
        Error::DuplicateTopologyId { node_id: 0 }
    );
    apply(&mut model, &first);
    assert_rejected!(
        model,
        group_end(/*group_id*/ 1),
        Error::TopologyKindMismatch {
            node_id: 1,
            expected: "group",
        }
    );
    let second = agent_begin(/*node_id*/ 2, /*parent_node_id*/ None);
    apply(&mut model, &second);
    for (event, expected) in [
        (
            bound(/*node_id*/ 99, /*attempt*/ 0, THREAD_A),
            Error::UnknownAgent { node_id: 99 },
        ),
        (
            bound(/*node_id*/ 0, /*attempt*/ 0, THREAD_A),
            Error::TopologyKindMismatch {
                node_id: 0,
                expected: "agent",
            },
        ),
        (
            bound(/*node_id*/ 1, /*attempt*/ 0, "not-a-uuid"),
            Error::InvalidChildThreadId { node_id: 1 },
        ),
        (
            bound(/*node_id*/ 1, /*attempt*/ 1, THREAD_A),
            unexpected_attempt(/*node_id*/ 1, /*expected*/ 0, /*actual*/ 1),
        ),
    ] {
        assert_rejected!(model, event, expected);
    }
    let binding = bound(/*node_id*/ 1, /*attempt*/ 0, THREAD_A);
    apply(&mut model, &binding);
    let before_duplicate = model.clone();
    apply(&mut model, &binding);
    assert_eq!(model, before_duplicate);
    let thread_a = ThreadId::from_string(THREAD_A).expect("valid thread");
    let thread_b = ThreadId::from_string(THREAD_B).expect("valid thread");
    assert_rejected!(
        model,
        bound(/*node_id*/ 1, /*attempt*/ 0, THREAD_B),
        Error::ConflictingAgentBinding {
            node_id: 1,
            existing_child_thread_id: thread_a,
            child_thread_id: thread_b,
        }
    );
    assert_rejected!(
        model,
        bound(/*node_id*/ 2, /*attempt*/ 0, THREAD_A),
        Error::ChildThreadAlreadyBound {
            node_id: 2,
            existing_node_id: 1,
            child_thread_id: thread_a,
        }
    );
    apply(&mut model, &retry(&first, /*attempt*/ 1));
    assert_rejected!(
        model,
        bound(/*node_id*/ 1, /*attempt*/ 0, THREAD_B),
        unexpected_attempt(/*node_id*/ 1, /*expected*/ 1, /*actual*/ 0)
    );
    apply(&mut model, &bound(/*node_id*/ 1, /*attempt*/ 1, THREAD_B));
    apply(&mut model, &bound(/*node_id*/ 2, /*attempt*/ 0, THREAD_A));
    let WorkflowTopologyNode::Agent(agent) = &model.topology[&1] else {
        panic!("node one should be an agent")
    };
    assert_eq!(agent.child_thread_id, Some(thread_b));
}

fn run_begin() -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "agent-audit".to_string(),
        phases: vec!["plan".to_string()],
        args_digest: "blake3:agents".to_string(),
    })
}

fn phase_begin() -> WorkflowEvent {
    WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
        run_id: RUN_ID.to_string(),
        phase_index: 0,
        title: "plan".to_string(),
    })
}

fn phase_end() -> WorkflowEvent {
    WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
        run_id: RUN_ID.to_string(),
        phase_index: 0,
        title: "plan".to_string(),
    })
}

fn group_begin(group_id: u64, parent_node_id: Option<u64>) -> WorkflowEvent {
    WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        parent_node_id,
        kind: WorkflowGroupKind::Parallel,
        item_count: 1,
    })
}

fn group_end(group_id: u64) -> WorkflowEvent {
    WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        kind: WorkflowGroupKind::Parallel,
        item_count: 1,
    })
}

fn agent_begin(node_id: u64, parent_node_id: Option<u64>) -> WorkflowEvent {
    WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        parent_node_id,
        label: format!("agent-{node_id}"),
        phase: None,
        model: "gpt-5".to_string(),
        effort: ReasoningEffort::High,
    })
}

fn retry(initial: &WorkflowEvent, attempt: u32) -> WorkflowEvent {
    let mut event = initial.clone();
    let agent = agent_event_mut(&mut event);
    agent.attempt = attempt;
    agent.last_attempt_reason = Some(WorkflowAgentAttemptReason::UserRetry);
    event
}

fn bound(node_id: u64, attempt: u32, child_thread_id: &str) -> WorkflowEvent {
    WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt,
        child_thread_id: child_thread_id.to_string(),
    })
}

fn agent_event_mut(event: &mut WorkflowEvent) -> &mut WorkflowAgentBeginEvent {
    let WorkflowEvent::AgentBegin(event) = event else {
        panic!("expected agent begin")
    };
    event
}

fn unexpected_attempt(node_id: u64, expected: u32, actual: u32) -> Error {
    Error::UnexpectedAgentAttempt {
        node_id,
        expected,
        actual,
    }
}

fn started_phase() -> WorkflowRunModel {
    project(&[run_begin(), phase_begin()])
}

fn project(events: &[WorkflowEvent]) -> WorkflowRunModel {
    let (first, events) = events.split_first().expect("projection needs run begin");
    let mut model = WorkflowRunModel::from_event(first).expect("run should begin");
    for event in events {
        apply(&mut model, event);
    }
    model
}

fn apply(model: &mut WorkflowRunModel, event: &WorkflowEvent) {
    assert_eq!(model.reduce_event(event), Ok(ReductionDisposition::Applied));
}

fn assert_rejected_unchanged(model: &mut WorkflowRunModel, event: &WorkflowEvent, expected: Error) {
    let before = model.clone();
    assert_eq!(model.reduce_event(event), Err(expected));
    assert_eq!(*model, before);
}

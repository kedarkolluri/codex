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
use pretty_assertions::assert_eq;

use super::*;

const RUN_ID: &str = "run-aggregates";
const THREAD_A: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a7";
const THREAD_B: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a8";
const THREAD_C: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a9";

macro_rules! usage {
    ($input:expr, $cached:expr, $written:expr, $output:expr, $reasoning:expr, $total:expr) => {
        TokenUsage {
            input_tokens: $input,
            cached_input_tokens: $cached,
            cache_write_input_tokens: $written,
            output_tokens: $output,
            reasoning_output_tokens: $reasoning,
            total_tokens: $total,
        }
    };
}

macro_rules! aggregate {
    ($groups:expr, $agents:expr, $active:expr, $completed:expr, $nulls:expr, $usage:expr, $tools:expr, $duration:expr) => {
        WorkflowAggregate {
            group_count: $groups,
            agent_count: $agents,
            active_agent_count: $active,
            completed_agent_count: $completed,
            returned_null_count: $nulls,
            token_usage: $usage,
            tool_call_count: $tools,
            duration_ms: $duration,
        }
    };
}

macro_rules! begin_agent {
    ($node:expr, $parent:expr, $phase:expr, $attempt:expr, $reason:expr) => {
        agent_begin($node, $parent, $phase, $attempt, $reason)
    };
}

macro_rules! update_agent {
    ($node:expr, $attempt:expr, $reason:expr, $usage:expr, $tools:expr, $duration:expr) => {
        agent_update($node, $attempt, $reason, ($usage, $tools, $duration))
    };
}

macro_rules! end_agent {
    ($node:expr, $attempt:expr, $reason:expr, $status:expr, $usage:expr, $tools:expr, $duration:expr, $null:expr) => {
        agent_end(
            $node,
            $attempt,
            $reason,
            $status,
            ($usage, $tools, $duration),
            $null,
        )
    };
}

#[test]
fn nested_multi_phase_aggregates_track_latest_agent_snapshots() {
    let usage_a = usage!(2, 3, 5, 7, 11, 13);
    let usage_b = usage!(17, 19, 23, 29, 31, 37);
    let usage_c = usage!(41, 43, 47, 53, 59, 61);
    let mut model = project(&[
        run_begin(&["plan", "verify"]),
        phase_begin(/*phase_index*/ 0, "plan"),
        group_begin(/*group_id*/ 0, /*parent_node_id*/ None),
        begin_agent!(1, Some(0), "plan", 0, None),
        bound(/*node_id*/ 1, /*attempt*/ 0, THREAD_A),
        begin_agent!(2, Some(1), "plan", 0, None),
        bound(/*node_id*/ 2, /*attempt*/ 0, THREAD_B),
        end_agent!(
            2,
            0,
            None,
            AgentStatus::Errored("child failed".to_string()),
            usage_b,
            79,
            83,
            true
        ),
        end_agent!(
            1,
            0,
            None,
            AgentStatus::Completed(None),
            usage_a,
            71,
            73,
            false
        ),
        group_end(/*group_id*/ 0),
        phase_end(/*phase_index*/ 0, "plan"),
    ]);

    let phase_zero = aggregate!(1, 2, 0, 2, 1, usage!(19, 22, 28, 36, 42, 50), 150, 156);
    assert_eq!(model.phases()[0].aggregate(), &phase_zero);

    for event in [
        phase_begin(/*phase_index*/ 1, "verify"),
        begin_agent!(3, None, "verify", 0, None),
        bound(/*node_id*/ 3, /*attempt*/ 0, THREAD_C),
        end_agent!(3, 0, None, AgentStatus::Interrupted, usage_c, 89, 97, false),
        phase_end(/*phase_index*/ 1, "verify"),
    ] {
        apply(&mut model, &event);
    }

    assert_eq!(model.phases()[0].aggregate(), &phase_zero);
    assert_eq!(
        model.phases()[1].aggregate(),
        &aggregate!(0, 1, 0, 1, 0, usage!(41, 43, 47, 53, 59, 61), 89, 97)
    );
    assert_eq!(
        model.aggregate(),
        &aggregate!(1, 3, 0, 3, 1, usage!(60, 65, 75, 89, 101, 111), 239, 253)
    );
}

#[test]
fn retry_and_non_counter_events_preserve_one_logical_agent() {
    let usage_a = usage!(2, 3, 5, 7, 11, 13);
    let usage_b = usage!(17, 19, 23, 29, 31, 37);
    let mut model = started_bound_agent();
    let update = update_agent!(0, 0, None, usage_a.clone(), 41, 43);
    apply(&mut model, &update);
    apply(&mut model, &update);
    let initial = aggregate!(0, 1, 1, 0, 0, usage_a, 41, 43);
    apply(&mut model, &bound(/*node_id*/ 0, /*attempt*/ 0, THREAD_A));
    apply(&mut model, &log_event());
    apply(
        &mut model,
        &begin_agent!(
            0,
            None,
            "work",
            1,
            Some(WorkflowAgentAttemptReason::UserRetry)
        ),
    );
    assert_eq!(model.aggregate(), &initial);

    apply(&mut model, &bound(/*node_id*/ 0, /*attempt*/ 1, THREAD_B));
    apply(
        &mut model,
        &update_agent!(
            0,
            1,
            Some(WorkflowAgentAttemptReason::UserRetry),
            usage_b.clone(),
            47,
            53
        ),
    );
    assert_eq!(
        model.aggregate(),
        &aggregate!(0, 1, 1, 0, 0, usage_b.clone(), 47, 53)
    );
    apply(
        &mut model,
        &end_agent!(
            0,
            1,
            Some(WorkflowAgentAttemptReason::UserRetry),
            AgentStatus::Shutdown,
            usage_b.clone(),
            47,
            53,
            true
        ),
    );
    let completed = aggregate!(0, 1, 0, 1, 1, usage_b, 47, 53);
    assert_eq!(model.aggregate(), &completed);
    assert_eq!(model.phases()[0].aggregate(), &completed);
}

#[test]
fn aggregate_overflow_rejects_every_partial_mutation() {
    let mut run_token_overflow = started_bound_agent();
    run_token_overflow.aggregate.token_usage.total_tokens = i64::MAX;
    assert_rejected_unchanged(
        &mut run_token_overflow,
        &update_agent!(0, 0, None, usage!(0, 0, 0, 0, 0, 1), 0, 0),
        WorkflowModelError::AggregateOverflow {
            field: "total_tokens",
        },
    );

    let mut phase_duration_overflow = started_bound_agent();
    phase_duration_overflow.phases[0].aggregate.duration_ms = u64::MAX;
    assert_rejected_unchanged(
        &mut phase_duration_overflow,
        &update_agent!(0, 0, None, TokenUsage::default(), 0, 1),
        WorkflowModelError::AggregateOverflow {
            field: "duration_ms",
        },
    );

    let mut terminal_count_overflow = started_bound_agent();
    terminal_count_overflow.aggregate.returned_null_count = u64::MAX;
    assert_rejected_unchanged(
        &mut terminal_count_overflow,
        &end_agent!(
            0,
            0,
            None,
            AgentStatus::Completed(None),
            TokenUsage::default(),
            0,
            0,
            true
        ),
        WorkflowModelError::AggregateOverflow {
            field: "returned_null_count",
        },
    );

    let mut topology_count_overflow = base_model();
    topology_count_overflow.phases[0].aggregate.group_count = u64::MAX;
    assert_rejected_unchanged(
        &mut topology_count_overflow,
        &group_begin(/*group_id*/ 0, /*parent_node_id*/ None),
        WorkflowModelError::AggregateOverflow {
            field: "group_count",
        },
    );
}

fn run_begin(phases: &[&str]) -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "aggregate-audit".to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        args_digest: "blake3:aggregates".to_string(),
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

fn group_begin(group_id: u64, parent_node_id: Option<u64>) -> WorkflowEvent {
    WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        parent_node_id,
        kind: WorkflowGroupKind::Parallel,
        item_count: 2,
    })
}

fn group_end(group_id: u64) -> WorkflowEvent {
    WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        kind: WorkflowGroupKind::Parallel,
        item_count: 2,
    })
}

fn agent_begin(
    node_id: u64,
    parent_node_id: Option<u64>,
    phase: &str,
    attempt: u32,
    last_attempt_reason: Option<WorkflowAgentAttemptReason>,
) -> WorkflowEvent {
    WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt,
        last_attempt_reason,
        parent_node_id,
        label: format!("agent-{node_id}"),
        phase: Some(phase.to_string()),
        model: "gpt-5".to_string(),
        effort: ReasoningEffort::High,
    })
}

fn bound(node_id: u64, attempt: u32, child_thread_id: &str) -> WorkflowEvent {
    WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt,
        child_thread_id: child_thread_id.to_string(),
    })
}

fn agent_update(
    node_id: u64,
    attempt: u32,
    last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    counters: (TokenUsage, u64, u64),
) -> WorkflowEvent {
    let (token_usage, tool_call_count, duration_ms) = counters;
    WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt,
        last_attempt_reason,
        token_usage,
        tool_call_count,
        duration_ms,
    })
}

fn agent_end(
    node_id: u64,
    attempt: u32,
    last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    status: AgentStatus,
    counters: (TokenUsage, u64, u64),
    returned_null: bool,
) -> WorkflowEvent {
    let (token_usage, tool_call_count, duration_ms) = counters;
    WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt,
        last_attempt_reason,
        status,
        token_usage,
        tool_call_count,
        duration_ms,
        returned_null,
    })
}

fn log_event() -> WorkflowEvent {
    WorkflowEvent::Log(WorkflowLogEvent {
        run_id: RUN_ID.to_string(),
        message: "still working".to_string(),
    })
}

fn base_model() -> WorkflowRunModel {
    project(&[run_begin(&["work"]), phase_begin(/*phase_index*/ 0, "work")])
}

fn started_bound_agent() -> WorkflowRunModel {
    let mut model = base_model();
    apply(&mut model, &begin_agent!(0, None, "work", 0, None));
    apply(&mut model, &bound(/*node_id*/ 0, /*attempt*/ 0, THREAD_A));
    model
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

fn assert_rejected_unchanged(
    model: &mut WorkflowRunModel,
    event: &WorkflowEvent,
    expected: WorkflowModelError,
) {
    let before = model.clone();
    assert_eq!(model.reduce_event(event), Err(expected));
    assert_eq!(*model, before);
}

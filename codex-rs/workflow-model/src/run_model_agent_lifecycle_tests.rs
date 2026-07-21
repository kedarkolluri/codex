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
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use pretty_assertions::assert_eq;

use super::*;

type Error = WorkflowModelError;
type TokenField = (&'static str, fn(&mut TokenUsage, i64));
const RUN_ID: &str = "run-agent-lifecycle";
const THREAD_A: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a7";
const THREAD_B: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a8";

macro_rules! assert_rejected {
    ($model:expr, $event:expr, $error:expr) => {
        assert_rejected_unchanged(&mut $model, &$event, $error)
    };
}

macro_rules! usage {
    (base: $base:expr) => {
        TokenUsage {
            input_tokens: $base,
            cached_input_tokens: $base + 1,
            cache_write_input_tokens: $base + 2,
            output_tokens: $base + 3,
            reasoning_output_tokens: $base + 4,
            total_tokens: $base + 5,
        }
    };
}

macro_rules! update {
    (attempt: $attempt:expr, reason: $reason:expr, base: $base:expr, tools: $tools:expr, ms: $ms:expr) => {
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: RUN_ID.to_string(),
            node_id: 0,
            attempt: $attempt,
            last_attempt_reason: $reason,
            token_usage: usage!(base: $base),
            tool_call_count: $tools,
            duration_ms: $ms,
        })
    };
}

macro_rules! end {
    (node: $node_id:expr, attempt: $attempt:expr, base: $base:expr, tools: $tools:expr, ms: $ms:expr) => {
        WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id: RUN_ID.to_string(),
            node_id: $node_id,
            attempt: $attempt,
            last_attempt_reason: None,
            status: AgentStatus::Completed(None),
            token_usage: usage!(base: $base),
            tool_call_count: $tools,
            duration_ms: $ms,
            returned_null: false,
        })
    };
}

#[test]
fn updates_are_exact_monotonic_and_transactional() {
    let mut model = started_bound_agent();
    let first = update!(attempt: 0, reason: None, base: 10, tools: 20, ms: 30);
    apply(&mut model, &first);
    apply(&mut model, &first);
    let advanced = update!(attempt: 0, reason: None, base: 40, tools: 50, ms: 60);
    apply(&mut model, &advanced);

    let mut unknown = base_model();
    assert_rejected!(unknown, first, Error::UnknownAgent { node_id: 0 });
    let mut wrong_kind = base_model();
    apply(&mut wrong_kind, &group_begin());
    assert_rejected!(
        wrong_kind,
        first,
        Error::TopologyKindMismatch {
            node_id: 0,
            expected: "agent",
        }
    );
    let mut unbound = started_agent();
    assert_rejected!(unbound, first, Error::AgentNotBound { node_id: 0 });
    let wrong_attempt = update!(attempt: 1, reason: None, base: 40, tools: 50, ms: 60);
    assert_rejected!(
        model,
        wrong_attempt,
        Error::UnexpectedAgentAttempt {
            node_id: 0,
            expected: 0,
            actual: 1,
        }
    );
    let wrong_reason = update!(
        attempt: 0,
        reason: Some(WorkflowAgentAttemptReason::UserRetry),
        base: 40,
        tools: 50,
        ms: 60
    );
    assert_rejected!(
        model,
        wrong_reason,
        Error::AgentAttemptReasonMismatch { node_id: 0 }
    );

    for ((field, set), previous) in token_fields().into_iter().zip(40..=45) {
        let mut negative = advanced.clone();
        set(&mut updated_mut(&mut negative).token_usage, -1);
        assert_rejected!(
            model,
            negative,
            Error::NegativeTokenUsage {
                node_id: 0,
                field,
                value: -1,
            }
        );
        let mut regression = advanced.clone();
        set(&mut updated_mut(&mut regression).token_usage, 0);
        assert_rejected!(
            model,
            regression,
            Error::TokenCounterRegression {
                node_id: 0,
                field,
                previous,
                actual: 0,
            }
        );
    }
    for (field, event, previous, actual) in [
        (
            "tool_call_count",
            update!(attempt: 0, reason: None, base: 40, tools: 49, ms: 60),
            50,
            49,
        ),
        (
            "duration_ms",
            update!(attempt: 0, reason: None, base: 40, tools: 50, ms: 59),
            60,
            59,
        ),
    ] {
        assert_rejected!(
            model,
            event,
            Error::UnsignedCounterRegression {
                node_id: 0,
                field,
                previous,
                actual,
            }
        );
    }

    let mut large = started_bound_agent();
    apply(
        &mut large,
        &update!(attempt: 0, reason: None, base: 0, tools: u64::MAX, ms: u64::MAX),
    );
    assert_rejected!(
        large,
        update!(attempt: 0, reason: None, base: 0, tools: u64::MAX - 1, ms: u64::MAX),
        Error::UnsignedCounterRegression {
            node_id: 0,
            field: "tool_call_count",
            previous: u64::MAX,
            actual: u64::MAX - 1,
        }
    );
}

#[test]
fn end_is_terminal_ordered_and_preserves_final_reason() {
    let terminal = end!(node: 0, attempt: 0, base: 10, tools: 20, ms: 30);
    let mut unbound = started_agent();
    assert_rejected!(unbound, terminal, Error::AgentNotBound { node_id: 0 });
    for status in [AgentStatus::PendingInit, AgentStatus::Running] {
        let mut model = started_bound_agent();
        let mut event = terminal.clone();
        ended_mut(&mut event).status = status.clone();
        assert_rejected!(
            model,
            event,
            Error::NonTerminalStatus {
                node_id: Some(0),
                status,
            }
        );
    }
    let mut progressed = started_bound_agent();
    apply(
        &mut progressed,
        &update!(attempt: 0, reason: None, base: 20, tools: 30, ms: 40),
    );
    assert_rejected!(
        progressed,
        terminal,
        Error::TokenCounterRegression {
            node_id: 0,
            field: "input_tokens",
            previous: 20,
            actual: 10,
        }
    );

    for (reason, status, returned_null) in [
        (None, AgentStatus::Completed(None), false),
        (
            Some(WorkflowAgentAttemptReason::UserRetry),
            AgentStatus::Interrupted,
            false,
        ),
        (
            Some(WorkflowAgentAttemptReason::UserSkip),
            AgentStatus::Shutdown,
            true,
        ),
        (
            Some(WorkflowAgentAttemptReason::RetryLimitReached),
            AgentStatus::Errored(String::new()),
            true,
        ),
        (None, AgentStatus::NotFound, true),
    ] {
        let mut model = started_bound_agent();
        let mut event = terminal.clone();
        let end = ended_mut(&mut event);
        end.last_attempt_reason = reason;
        end.status = status.clone();
        end.returned_null = returned_null;
        apply(&mut model, &event);
        let WorkflowTopologyNode::Agent(agent) = &model.topology[&0] else {
            panic!("node zero should be an agent")
        };
        assert_eq!(
            (
                agent.state(),
                agent.status(),
                agent.last_attempt_reason(),
                agent.returned_null(),
                agent.token_usage(),
                agent.tool_call_count(),
                agent.duration_ms(),
            ),
            (
                WorkflowNodeState::Completed,
                &status,
                reason,
                returned_null,
                &usage!(base: 10),
                20,
                30,
            )
        );
        assert_rejected!(model, event, Error::NodeAlreadyCompleted { node_id: 0 });
        assert_rejected!(
            model,
            update!(attempt: 0, reason: reason, base: 10, tools: 20, ms: 30),
            Error::NodeAlreadyCompleted { node_id: 0 }
        );
    }

    let mut parent = started_bound_agent();
    apply(
        &mut parent,
        &agent_begin(/*node_id*/ 1, /*parent_node_id*/ Some(0)),
    );
    apply(&mut parent, &bound(/*node_id*/ 1, /*attempt*/ 0, THREAD_B));
    assert_rejected!(
        parent,
        terminal,
        Error::ActiveChildAtAgentEnd {
            node_id: 0,
            child_node_id: 1,
        }
    );
    apply(
        &mut parent,
        &end!(node: 1, attempt: 0, base: 0, tools: 0, ms: 0),
    );
    apply(&mut parent, &terminal);
}

#[test]
fn retried_live_and_compact_streams_are_deeply_equal() {
    let initial = agent_begin(/*node_id*/ 0, /*parent_node_id*/ None);
    let retry = retry(&initial, /*attempt*/ 1);
    let cumulative = update!(
        attempt: 1,
        reason: Some(WorkflowAgentAttemptReason::UserRetry),
        base: 40,
        tools: 50,
        ms: 60
    );
    let mut terminal = end!(node: 0, attempt: 1, base: 70, tools: 80, ms: 90);
    ended_mut(&mut terminal).last_attempt_reason = Some(WorkflowAgentAttemptReason::UserRetry);
    let live = project(&[
        run_begin(),
        phase_begin(),
        initial.clone(),
        bound(/*node_id*/ 0, /*attempt*/ 0, THREAD_A),
        update!(attempt: 0, reason: None, base: 10, tools: 20, ms: 30),
        retry.clone(),
        bound(/*node_id*/ 0, /*attempt*/ 1, THREAD_B),
        cumulative.clone(),
        terminal.clone(),
    ]);
    let compact = project(&[
        run_begin(),
        phase_begin(),
        initial,
        retry,
        bound(/*node_id*/ 0, /*attempt*/ 1, THREAD_B),
        cumulative,
        terminal,
    ]);
    assert_eq!(live, compact);
}

fn run_begin() -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "agent-lifecycle".to_string(),
        phases: vec!["work".to_string()],
        args_digest: "blake3:lifecycle".to_string(),
    })
}

fn phase_begin() -> WorkflowEvent {
    WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
        run_id: RUN_ID.to_string(),
        phase_index: 0,
        title: "work".to_string(),
    })
}

fn group_begin() -> WorkflowEvent {
    WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
        run_id: RUN_ID.to_string(),
        group_id: 0,
        parent_node_id: None,
        kind: WorkflowGroupKind::Parallel,
        item_count: 0,
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
        phase: Some("work".to_string()),
        model: "gpt-5".to_string(),
        effort: ReasoningEffort::High,
    })
}

fn retry(initial: &WorkflowEvent, attempt: u32) -> WorkflowEvent {
    let mut event = initial.clone();
    let WorkflowEvent::AgentBegin(agent) = &mut event else {
        panic!("expected agent begin")
    };
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

fn token_fields() -> [TokenField; 6] {
    [
        ("input_tokens", |usage, value| usage.input_tokens = value),
        ("cached_input_tokens", |usage, value| {
            usage.cached_input_tokens = value
        }),
        ("cache_write_input_tokens", |usage, value| {
            usage.cache_write_input_tokens = value
        }),
        ("output_tokens", |usage, value| usage.output_tokens = value),
        ("reasoning_output_tokens", |usage, value| {
            usage.reasoning_output_tokens = value
        }),
        ("total_tokens", |usage, value| usage.total_tokens = value),
    ]
}

fn updated_mut(event: &mut WorkflowEvent) -> &mut WorkflowAgentUpdatedEvent {
    let WorkflowEvent::AgentUpdated(event) = event else {
        panic!("expected agent update")
    };
    event
}

fn ended_mut(event: &mut WorkflowEvent) -> &mut WorkflowAgentEndEvent {
    let WorkflowEvent::AgentEnd(event) = event else {
        panic!("expected agent end")
    };
    event
}

fn base_model() -> WorkflowRunModel {
    project(&[run_begin(), phase_begin()])
}

fn started_agent() -> WorkflowRunModel {
    project(&[
        run_begin(),
        phase_begin(),
        agent_begin(/*node_id*/ 0, /*parent_node_id*/ None),
    ])
}

fn started_bound_agent() -> WorkflowRunModel {
    let mut model = started_agent();
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
    assert_eq!(model.apply(event), Ok(()));
}

fn assert_rejected_unchanged(model: &mut WorkflowRunModel, event: &WorkflowEvent, expected: Error) {
    let before = model.clone();
    assert_eq!(model.apply(event), Err(expected));
    assert_eq!(*model, before);
}

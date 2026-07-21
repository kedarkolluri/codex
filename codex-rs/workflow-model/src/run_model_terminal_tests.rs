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
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason as RunReason;
use pretty_assertions::assert_eq;

use super::*;

const RUN_ID: &str = "run-terminal";
const THREAD_ID: &str = "019b0214-7c3f-7d80-bd83-88cb759c24a7";
type Error = WorkflowModelError;

#[test]
fn public_apply_is_exhaustive_and_replays_deeply_equal() {
    let events = vec![
        run_begin(&["work", "verify", "later"]),
        WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: RUN_ID.to_string(),
            phase_index: 0,
            title: "work".to_string(),
        }),
        log("starting"),
        group_begin(
            /*group_id*/ 0, /*parent_node_id*/ None, /*item_count*/ 1,
        ),
        agent_begin(
            /*node_id*/ 1,
            /*parent_node_id*/ Some(0),
            /*phase*/ Some("work"),
        ),
        agent_bound(/*node_id*/ 1),
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: RUN_ID.to_string(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: usage(/*value*/ 1),
            tool_call_count: 1,
            duration_ms: 2,
        }),
        agent_end(
            /*node_id*/ 1,
            AgentStatus::Completed(None),
            usage(/*value*/ 2),
        ),
        group_end(/*group_id*/ 0, /*item_count*/ 1),
        WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
            run_id: RUN_ID.to_string(),
            phase_index: 0,
            title: "work".to_string(),
        }),
        WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: RUN_ID.to_string(),
            phase_index: 1,
            title: "verify".to_string(),
        }),
        WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
            run_id: RUN_ID.to_string(),
            phase_index: 1,
            title: "verify".to_string(),
        }),
        run_end(
            AgentStatus::Completed(Some("done".to_string())),
            Some(RunReason::Completed),
            /*spent*/ 7,
            /*total*/ Some(10),
        ),
    ];

    let first = project(&events);
    let second = project(&events);
    assert_eq!(first, second);
    assert_eq!(
        (
            first.state(),
            first.status().clone(),
            first.terminal_reason(),
            first
                .budget()
                .map(|budget| (budget.spent(), budget.total())),
            first
                .phases()
                .iter()
                .map(WorkflowPhase::state)
                .collect::<Vec<_>>(),
            first
                .topology()
                .values()
                .map(WorkflowTopologyNode::state)
                .collect::<Vec<_>>(),
        ),
        (
            WorkflowRunState::Completed,
            AgentStatus::Completed(Some("done".to_string())),
            Some(RunReason::Completed),
            Some((7, Some(10))),
            vec![
                WorkflowPhaseState::Completed,
                WorkflowPhaseState::Completed,
                WorkflowPhaseState::Pending,
            ],
            vec![WorkflowNodeState::Completed; 2],
        )
    );

    let mut completed = first;
    assert_unchanged(&mut completed, &events[0], Error::DuplicateRunBegin);
}

#[test]
fn terminal_budget_preserves_absence_unmetered_and_zero_total() {
    assert_eq!(base_model().budget(), None);
    for (spent, total) in [(0, None), (0, Some(0)), (i64::MAX, Some(0))] {
        let mut model = base_model();
        apply_ok(&mut model, &completed_run_end(spent, total));
        assert_eq!(
            model
                .budget()
                .map(|budget| (budget.spent(), budget.total())),
            Some((spent, total))
        );
        assert_eq!(model.phases()[0].state(), WorkflowPhaseState::Completed);
    }

    for (spent, total) in [(-1, None), (0, Some(-1)), (-1, Some(-1))] {
        let mut model = base_model();
        assert_unchanged(
            &mut model,
            &completed_run_end(spent, total),
            Error::NegativeBudget { spent, total },
        );
    }
}

#[test]
fn terminal_status_reason_matrix_is_exact_and_legacy_compatible() {
    for status in [
        AgentStatus::Interrupted,
        AgentStatus::Completed(None),
        AgentStatus::Completed(Some(String::new())),
        AgentStatus::Errored(String::new()),
        AgentStatus::Shutdown,
        AgentStatus::NotFound,
    ] {
        let mut model = base_model();
        apply_ok(
            &mut model,
            &terminal(status.clone(), /*terminal_reason*/ None),
        );
        assert_eq!((model.status(), model.terminal_reason()), (&status, None));
    }

    let statuses = [
        AgentStatus::Completed(None),
        AgentStatus::Errored("failed".to_string()),
        AgentStatus::Interrupted,
        AgentStatus::Shutdown,
        AgentStatus::NotFound,
    ];
    let reasons = [
        RunReason::Completed,
        RunReason::Failed,
        RunReason::Interrupted,
        RunReason::Stopped,
        RunReason::Paused,
    ];
    for status in statuses {
        for terminal_reason in reasons {
            let compatible = match terminal_reason {
                RunReason::Completed => {
                    matches!(&status, AgentStatus::Completed(_))
                }
                RunReason::Failed => {
                    matches!(&status, AgentStatus::Errored(_))
                }
                RunReason::Interrupted | RunReason::Paused => status == AgentStatus::Interrupted,
                RunReason::Stopped => status == AgentStatus::Shutdown,
            };
            let event = terminal(status.clone(), Some(terminal_reason));
            let mut model = base_model();
            if compatible {
                apply_ok(&mut model, &event);
                assert_eq!(model.terminal_reason(), Some(terminal_reason));
            } else {
                assert_unchanged(
                    &mut model,
                    &event,
                    Error::TerminalReasonMismatch {
                        status: status.clone(),
                        terminal_reason,
                    },
                );
            }
        }
    }

    for status in [AgentStatus::PendingInit, AgentStatus::Running] {
        let mut model = base_model();
        assert_unchanged(
            &mut model,
            &terminal(status.clone(), /*terminal_reason*/ None),
            Error::NonTerminalStatus {
                node_id: None,
                status,
            },
        );
    }
}

#[test]
fn run_end_rejects_active_topology_and_post_terminal_events() {
    let terminal = completed_run_end(/*spent*/ 0, /*total*/ None);
    let mut model = base_model();
    apply_ok(
        &mut model,
        &group_begin(
            /*group_id*/ 0, /*parent_node_id*/ None, /*item_count*/ 0,
        ),
    );
    assert_unchanged(
        &mut model,
        &terminal,
        Error::ActiveTopologyAtRunEnd { node_id: 0 },
    );
    apply_ok(&mut model, &group_end(/*group_id*/ 0, /*item_count*/ 0));
    apply_ok(
        &mut model,
        &agent_begin(
            /*node_id*/ 1, /*parent_node_id*/ None, /*phase*/ None,
        ),
    );
    apply_ok(&mut model, &agent_bound(/*node_id*/ 1));
    assert_unchanged(
        &mut model,
        &terminal,
        Error::ActiveTopologyAtRunEnd { node_id: 1 },
    );
    apply_ok(
        &mut model,
        &agent_end(
            /*node_id*/ 1,
            AgentStatus::Completed(None),
            TokenUsage::default(),
        ),
    );
    apply_ok(&mut model, &terminal);

    assert_unchanged(&mut model, &log("late"), Error::RunAlreadyCompleted);
    assert_unchanged(&mut model, &terminal, Error::RunAlreadyCompleted);
}

#[test]
fn terminal_status_messages_have_exact_utf8_byte_bounds() {
    let exact = "é".repeat(WORKFLOW_STATUS_MESSAGE_MAX_BYTES / "é".len());
    let over = format!("{exact}x");

    for (status, terminal_reason) in [
        (
            AgentStatus::Completed(Some(exact.clone())),
            RunReason::Completed,
        ),
        (AgentStatus::Errored(exact.clone()), RunReason::Failed),
    ] {
        let mut model = base_model();
        apply_ok(&mut model, &terminal(status.clone(), Some(terminal_reason)));
        assert_eq!(model.status(), &status);
    }
    for (status, incompatible_reason) in [
        (
            AgentStatus::Completed(Some(over.clone())),
            RunReason::Failed,
        ),
        (AgentStatus::Errored(over.clone()), RunReason::Completed),
    ] {
        let mut model = base_model();
        assert_unchanged(
            &mut model,
            &terminal(status, Some(incompatible_reason)),
            Error::TextTooLong {
                field: "run status message",
                maximum_bytes: WORKFLOW_STATUS_MESSAGE_MAX_BYTES,
                actual_bytes: WORKFLOW_STATUS_MESSAGE_MAX_BYTES + 1,
            },
        );
    }

    for (status, accepted) in [
        (AgentStatus::Completed(Some(exact.clone())), true),
        (AgentStatus::Errored(exact), true),
        (AgentStatus::Completed(Some(over.clone())), false),
        (AgentStatus::Errored(over), false),
    ] {
        let mut model = started_bound_agent();
        let event = agent_end(/*node_id*/ 0, status.clone(), TokenUsage::default());
        if accepted {
            apply_ok(&mut model, &event);
            let WorkflowTopologyNode::Agent(agent) = &model.topology()[&0] else {
                panic!("node zero should be an agent")
            };
            assert_eq!(agent.status(), &status);
        } else {
            assert_unchanged(
                &mut model,
                &event,
                Error::TextTooLong {
                    field: "agent status message",
                    maximum_bytes: WORKFLOW_STATUS_MESSAGE_MAX_BYTES,
                    actual_bytes: WORKFLOW_STATUS_MESSAGE_MAX_BYTES + 1,
                },
            );
        }
    }
}

fn run_begin(phases: &[&str]) -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "terminal-audit".to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        args_digest: "blake3:terminal".to_string(),
    })
}

fn run_end(
    status: AgentStatus,
    terminal_reason: Option<RunReason>,
    spent: i64,
    total: Option<i64>,
) -> WorkflowEvent {
    WorkflowEvent::RunEnd(WorkflowRunEndEvent {
        run_id: RUN_ID.to_string(),
        status,
        terminal_reason,
        spent,
        total,
    })
}

fn completed_run_end(spent: i64, total: Option<i64>) -> WorkflowEvent {
    let reason = Some(RunReason::Completed);
    run_end(AgentStatus::Completed(None), reason, spent, total)
}

fn terminal(status: AgentStatus, terminal_reason: Option<RunReason>) -> WorkflowEvent {
    run_end(
        status,
        terminal_reason,
        /*spent*/ 0,
        /*total*/ None,
    )
}

fn log(message: &str) -> WorkflowEvent {
    WorkflowEvent::Log(WorkflowLogEvent {
        run_id: RUN_ID.to_string(),
        message: message.to_string(),
    })
}

fn group_begin(group_id: u64, parent_node_id: Option<u64>, item_count: u64) -> WorkflowEvent {
    WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        parent_node_id,
        kind: WorkflowGroupKind::Parallel,
        item_count,
    })
}

fn group_end(group_id: u64, item_count: u64) -> WorkflowEvent {
    WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
        run_id: RUN_ID.to_string(),
        group_id,
        kind: WorkflowGroupKind::Parallel,
        item_count,
    })
}

fn agent_begin(node_id: u64, parent_node_id: Option<u64>, phase: Option<&str>) -> WorkflowEvent {
    WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        parent_node_id,
        label: format!("agent-{node_id}"),
        phase: phase.map(ToString::to_string),
        model: "gpt-5".to_string(),
        effort: ReasoningEffort::High,
    })
}

fn agent_bound(node_id: u64) -> WorkflowEvent {
    WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        child_thread_id: THREAD_ID.to_string(),
    })
}

fn agent_end(node_id: u64, status: AgentStatus, token_usage: TokenUsage) -> WorkflowEvent {
    WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
        run_id: RUN_ID.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        status,
        token_usage,
        tool_call_count: 2,
        duration_ms: 3,
        returned_null: false,
    })
}

fn usage(value: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: value,
        cached_input_tokens: value,
        cache_write_input_tokens: value,
        output_tokens: value,
        reasoning_output_tokens: value,
        total_tokens: value,
    }
}

fn base_model() -> WorkflowRunModel {
    WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin")
}

fn started_bound_agent() -> WorkflowRunModel {
    let mut model = base_model();
    apply_ok(
        &mut model,
        &agent_begin(
            /*node_id*/ 0, /*parent_node_id*/ None, /*phase*/ None,
        ),
    );
    apply_ok(&mut model, &agent_bound(/*node_id*/ 0));
    model
}

fn project(events: &[WorkflowEvent]) -> WorkflowRunModel {
    let (first, events) = events.split_first().expect("projection needs run begin");
    let mut model = WorkflowRunModel::from_event(first).expect("run should begin");
    for event in events {
        apply_ok(&mut model, event);
    }
    model
}

fn apply_ok(model: &mut WorkflowRunModel, event: &WorkflowEvent) {
    assert_eq!(model.apply(event), Ok(()));
}

fn assert_unchanged(model: &mut WorkflowRunModel, event: &WorkflowEvent, expected: Error) {
    let before = model.clone();
    assert_eq!(model.apply(event), Err(expected));
    assert_eq!(*model, before);
}

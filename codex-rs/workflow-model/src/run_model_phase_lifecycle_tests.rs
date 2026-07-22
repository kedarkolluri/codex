use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use pretty_assertions::assert_eq;

use super::*;

const RUN_ID: &str = "run-phases";

fn run_begin(phases: &[&str]) -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "phase-audit".to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        args_digest: "blake3:phase-args".to_string(),
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

fn log_event(message: &str) -> WorkflowEvent {
    WorkflowEvent::Log(WorkflowLogEvent {
        run_id: RUN_ID.to_string(),
        message: message.to_string(),
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

fn assert_rejected_unchanged(
    model: &mut WorkflowRunModel,
    event: &WorkflowEvent,
    expected: WorkflowModelError,
) {
    let before = model.clone();
    assert_eq!(model.apply(event), Err(expected));
    assert_eq!(*model, before);
}

#[test]
fn declared_and_dynamic_phases_reduce_deterministically() {
    let events = vec![
        run_begin(&["plan"]),
        phase_begin(/*phase_index*/ 0, "plan"),
        phase_end(/*phase_index*/ 0, "plan"),
        phase_begin(/*phase_index*/ 1, "verify"),
        log_event("checking"),
        phase_end(/*phase_index*/ 1, "verify"),
    ];

    let first = project(&events).expect("valid phase lifecycle should project");
    let second = project(&events).expect("replayed phase lifecycle should project");
    assert_eq!(first, second);
    assert_eq!(
        first,
        WorkflowRunModel {
            run_id: RUN_ID.to_string(),
            resumed_from_run_id: None,
            name: "phase-audit".to_string(),
            args_digest: "blake3:phase-args".to_string(),
            state: WorkflowRunState::Running,
            status: AgentStatus::Running,
            terminal_reason: None,
            budget: None,
            phases: vec![
                phase(
                    /*index*/ 0,
                    "plan",
                    WorkflowPhaseState::Completed,
                    /*implicit*/ false,
                ),
                phase(
                    /*index*/ 1,
                    "verify",
                    WorkflowPhaseState::Completed,
                    /*implicit*/ false,
                ),
            ],
            topology: BTreeMap::new(),
            aggregate: WorkflowAggregate::default(),
            active_phase_index: None,
            next_phase_index: 2,
            next_topology_id: 0,
            log_event_count: 1,
        }
    );
}

#[test]
fn first_explicit_phase_replaces_the_implicit_root() {
    let mut model = WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin");
    assert_rejected_unchanged(
        &mut model,
        &phase_end(/*phase_index*/ 0, "root"),
        WorkflowModelError::PhaseNotActive { phase_index: 0 },
    );

    assert_eq!(
        model.apply(&phase_begin(/*phase_index*/ 0, "discover")),
        Ok(())
    );
    assert_eq!(
        model.apply(&phase_end(/*phase_index*/ 0, "discover")),
        Ok(())
    );
    assert_eq!(
        model.apply(&phase_begin(/*phase_index*/ 1, "summarize")),
        Ok(())
    );

    assert_eq!(
        model,
        WorkflowRunModel {
            run_id: RUN_ID.to_string(),
            resumed_from_run_id: None,
            name: "phase-audit".to_string(),
            args_digest: "blake3:phase-args".to_string(),
            state: WorkflowRunState::Running,
            status: AgentStatus::Running,
            terminal_reason: None,
            budget: None,
            phases: vec![
                phase(
                    /*index*/ 0,
                    "discover",
                    WorkflowPhaseState::Completed,
                    /*implicit*/ false,
                ),
                phase(
                    /*index*/ 1,
                    "summarize",
                    WorkflowPhaseState::Active,
                    /*implicit*/ false,
                ),
            ],
            topology: BTreeMap::new(),
            aggregate: WorkflowAggregate::default(),
            active_phase_index: Some(1),
            next_phase_index: 2,
            next_topology_id: 0,
            log_event_count: 0,
        }
    );
}

#[test]
fn invalid_phase_ordering_is_transactional() {
    let mut model =
        WorkflowRunModel::from_event(&run_begin(&["plan", "execute"])).expect("run should begin");
    assert_rejected_unchanged(
        &mut model,
        &run_begin(&["duplicate"]),
        WorkflowModelError::DuplicateRunBegin,
    );
    assert_rejected_unchanged(
        &mut model,
        &phase_begin(/*phase_index*/ 1, "execute"),
        WorkflowModelError::UnexpectedPhaseIndex {
            expected: 0,
            actual: 1,
        },
    );
    assert_rejected_unchanged(
        &mut model,
        &phase_begin(/*phase_index*/ 0, "wrong"),
        WorkflowModelError::PhaseTitleMismatch {
            phase_index: 0,
            expected: "plan".to_string(),
            actual: "wrong".to_string(),
        },
    );
    assert_eq!(model.apply(&phase_begin(/*phase_index*/ 0, "plan")), Ok(()));
    assert_rejected_unchanged(
        &mut model,
        &phase_begin(/*phase_index*/ 1, "execute"),
        WorkflowModelError::PhaseAlreadyActive { phase_index: 0 },
    );
    assert_rejected_unchanged(
        &mut model,
        &phase_end(/*phase_index*/ 1, "execute"),
        WorkflowModelError::PhaseNotActive { phase_index: 1 },
    );
    assert_rejected_unchanged(
        &mut model,
        &phase_end(/*phase_index*/ 0, "wrong"),
        WorkflowModelError::PhaseTitleMismatch {
            phase_index: 0,
            expected: "plan".to_string(),
            actual: "wrong".to_string(),
        },
    );
    assert_eq!(model.apply(&phase_end(/*phase_index*/ 0, "plan")), Ok(()));
    assert_rejected_unchanged(
        &mut model,
        &phase_begin(/*phase_index*/ 0, "plan"),
        WorkflowModelError::UnexpectedPhaseIndex {
            expected: 1,
            actual: 0,
        },
    );
    assert_rejected_unchanged(
        &mut model,
        &phase_end(/*phase_index*/ 0, "plan"),
        WorkflowModelError::PhaseNotActive { phase_index: 0 },
    );
}

#[test]
fn event_headers_and_text_are_bounded_before_mutation() {
    let base = WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin");

    let mut wrong_run = phase_begin(/*phase_index*/ 0, "discover");
    let WorkflowEvent::PhaseBegin(event) = &mut wrong_run else {
        unreachable!("phase_begin helper returns phase_begin")
    };
    event.run_id = "another-run".to_string();
    let mut model = base.clone();
    assert_rejected_unchanged(
        &mut model,
        &wrong_run,
        WorkflowModelError::RunIdMismatch {
            expected: RUN_ID.to_string(),
            actual: "another-run".to_string(),
        },
    );

    let mut blank_run = phase_begin(/*phase_index*/ 0, "discover");
    let WorkflowEvent::PhaseBegin(event) = &mut blank_run else {
        unreachable!("phase_begin helper returns phase_begin")
    };
    event.run_id = " \n ".to_string();
    let mut model = base.clone();
    assert_rejected_unchanged(
        &mut model,
        &blank_run,
        WorkflowModelError::EmptyText { field: "run_id" },
    );

    let mut long_run = phase_begin(/*phase_index*/ 0, "discover");
    let WorkflowEvent::PhaseBegin(event) = &mut long_run else {
        unreachable!("phase_begin helper returns phase_begin")
    };
    event.run_id = "r".repeat(WORKFLOW_RUN_ID_MAX_BYTES + 1);
    let mut model = base.clone();
    assert_rejected_unchanged(
        &mut model,
        &long_run,
        WorkflowModelError::TextTooLong {
            field: "run_id",
            maximum_bytes: WORKFLOW_RUN_ID_MAX_BYTES,
            actual_bytes: WORKFLOW_RUN_ID_MAX_BYTES + 1,
        },
    );

    let invalid_phase_titles = || {
        [
            (
                " \n ".to_string(),
                WorkflowModelError::EmptyText {
                    field: "phase title",
                },
            ),
            (
                "p".repeat(WORKFLOW_PHASE_TITLE_MAX_BYTES + 1),
                WorkflowModelError::TextTooLong {
                    field: "phase title",
                    maximum_bytes: WORKFLOW_PHASE_TITLE_MAX_BYTES,
                    actual_bytes: WORKFLOW_PHASE_TITLE_MAX_BYTES + 1,
                },
            ),
        ]
    };
    for (title, expected) in invalid_phase_titles() {
        let mut model = base.clone();
        assert_rejected_unchanged(
            &mut model,
            &phase_begin(/*phase_index*/ 0, &title),
            expected,
        );
    }

    let mut active =
        WorkflowRunModel::from_event(&run_begin(&["plan"])).expect("declared run should begin");
    assert_eq!(
        active.apply(&phase_begin(/*phase_index*/ 0, "plan")),
        Ok(())
    );
    for (title, expected) in invalid_phase_titles() {
        assert_rejected_unchanged(&mut active, &phase_end(/*phase_index*/ 0, &title), expected);
    }

    for (message, expected) in [
        (
            " \n ".to_string(),
            WorkflowModelError::EmptyText {
                field: "log message",
            },
        ),
        (
            "l".repeat(WORKFLOW_LOG_MESSAGE_MAX_BYTES + 1),
            WorkflowModelError::TextTooLong {
                field: "log message",
                maximum_bytes: WORKFLOW_LOG_MESSAGE_MAX_BYTES,
                actual_bytes: WORKFLOW_LOG_MESSAGE_MAX_BYTES + 1,
            },
        ),
    ] {
        let mut model = base.clone();
        assert_rejected_unchanged(&mut model, &log_event(&message), expected);
    }

    let mut exact = base.clone();
    let exact_title = "p".repeat(WORKFLOW_PHASE_TITLE_MAX_BYTES);
    assert_eq!(
        exact.apply(&phase_begin(/*phase_index*/ 0, &exact_title)),
        Ok(())
    );
    assert_eq!(
        exact.apply(&log_event(&"l".repeat(WORKFLOW_LOG_MESSAGE_MAX_BYTES))),
        Ok(())
    );

    let mut completed = base;
    completed.state = WorkflowRunState::Completed;
    assert_rejected_unchanged(
        &mut completed,
        &log_event("late"),
        WorkflowModelError::RunAlreadyCompleted,
    );
}

#[test]
fn phase_and_log_caps_fail_without_mutation() {
    let mut phases = WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin");
    for phase_index in 0..WORKFLOW_PHASE_MAX_EVENTS {
        let title = format!("phase-{phase_index}");
        assert_eq!(phases.apply(&phase_begin(phase_index, &title)), Ok(()));
        assert_eq!(phases.apply(&phase_end(phase_index, &title)), Ok(()));
    }
    assert_eq!(
        phases.phases.len(),
        usize::try_from(WORKFLOW_PHASE_MAX_EVENTS).expect("phase cap fits usize")
    );
    assert_rejected_unchanged(
        &mut phases,
        &phase_begin(WORKFLOW_PHASE_MAX_EVENTS, "over-cap"),
        WorkflowModelError::PhaseLimitExceeded {
            maximum: WORKFLOW_PHASE_MAX_EVENTS,
        },
    );

    let mut logs = WorkflowRunModel::from_event(&run_begin(&[])).expect("run should begin");
    let event = log_event("bounded");
    for _ in 0..WORKFLOW_LOG_MAX_EVENTS {
        assert_eq!(logs.apply(&event), Ok(()));
    }
    assert_eq!(logs.log_event_count, WORKFLOW_LOG_MAX_EVENTS);
    assert_rejected_unchanged(
        &mut logs,
        &event,
        WorkflowModelError::LogLimitExceeded {
            maximum: WORKFLOW_LOG_MAX_EVENTS,
        },
    );
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

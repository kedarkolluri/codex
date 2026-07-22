use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use pretty_assertions::assert_eq;

use super::*;

fn run_begin(phases: &[&str]) -> WorkflowEvent {
    WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: "run-7".to_string(),
        resumed_from_run_id: Some("run-6".to_string()),
        name: "release-audit".to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        args_digest: "blake3:args".to_string(),
    })
}

#[test]
fn run_begin_seeds_every_declared_phase_pending() {
    let model = WorkflowRunModel::from_event(&run_begin(&["plan", "execute", "verify"]))
        .expect("run begin should create a model");

    assert_eq!(
        model,
        WorkflowRunModel {
            run_id: "run-7".to_string(),
            resumed_from_run_id: Some("run-6".to_string()),
            name: "release-audit".to_string(),
            args_digest: "blake3:args".to_string(),
            state: WorkflowRunState::Running,
            status: AgentStatus::Running,
            terminal_reason: None,
            budget: None,
            phases: vec![
                phase(
                    /*index*/ 0,
                    "plan",
                    WorkflowPhaseState::Pending,
                    /*implicit*/ false,
                ),
                phase(
                    /*index*/ 1,
                    "execute",
                    WorkflowPhaseState::Pending,
                    /*implicit*/ false,
                ),
                phase(
                    /*index*/ 2,
                    "verify",
                    WorkflowPhaseState::Pending,
                    /*implicit*/ false,
                ),
            ],
            topology: BTreeMap::new(),
            aggregate: WorkflowAggregate::default(),
            active_phase_index: None,
            next_phase_index: 0,
            next_topology_id: 0,
            log_event_count: 0,
        }
    );
}

#[test]
fn empty_phase_list_seeds_one_active_implicit_root() {
    let model =
        WorkflowRunModel::from_event(&run_begin(&[])).expect("run begin should create a model");

    assert_eq!(
        model,
        WorkflowRunModel {
            run_id: "run-7".to_string(),
            resumed_from_run_id: Some("run-6".to_string()),
            name: "release-audit".to_string(),
            args_digest: "blake3:args".to_string(),
            state: WorkflowRunState::Running,
            status: AgentStatus::Running,
            terminal_reason: None,
            budget: None,
            phases: vec![phase(
                /*index*/ 0,
                "root",
                WorkflowPhaseState::Active,
                /*implicit*/ true,
            )],
            topology: BTreeMap::new(),
            aggregate: WorkflowAggregate::default(),
            active_phase_index: Some(0),
            next_phase_index: 0,
            next_topology_id: 0,
            log_event_count: 0,
        }
    );
}

#[test]
fn model_requires_run_begin_as_its_first_event() {
    let event = WorkflowEvent::Log(WorkflowLogEvent {
        run_id: "run-7".to_string(),
        message: "too early".to_string(),
    });

    assert_eq!(
        WorkflowRunModel::from_event(&event),
        Err(WorkflowModelError::ExpectedRunBegin)
    );
}

#[test]
fn run_begin_rejects_unbounded_identity_and_phase_data() {
    let mut empty_run_id = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut empty_run_id else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.run_id.clear();

    let mut long_name = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut long_name else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.name = "n".repeat(codex_code_mode_protocol::WORKFLOW_NAME_MAX_BYTES + 1);

    let mut blank_name = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut blank_name else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.name = " \n ".to_string();

    let mut long_phase = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut long_phase else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.phases = vec!["p".repeat(codex_code_mode_protocol::WORKFLOW_PHASE_TITLE_MAX_BYTES + 1)];

    let mut blank_phase = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut blank_phase else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.phases = vec![" \n ".to_string()];

    let phase_limit =
        usize::try_from(codex_code_mode_protocol::WORKFLOW_PHASE_MAX_EVENTS).unwrap_or(usize::MAX);
    let too_many_phases = run_begin(&vec!["phase"; phase_limit + 1]);

    let cases = [
        (
            empty_run_id,
            WorkflowModelError::EmptyText { field: "run_id" },
        ),
        (
            long_name,
            WorkflowModelError::TextTooLong {
                field: "name",
                maximum_bytes: codex_code_mode_protocol::WORKFLOW_NAME_MAX_BYTES,
                actual_bytes: codex_code_mode_protocol::WORKFLOW_NAME_MAX_BYTES + 1,
            },
        ),
        (blank_name, WorkflowModelError::EmptyText { field: "name" }),
        (
            long_phase,
            WorkflowModelError::TextTooLong {
                field: "phase title",
                maximum_bytes: codex_code_mode_protocol::WORKFLOW_PHASE_TITLE_MAX_BYTES,
                actual_bytes: codex_code_mode_protocol::WORKFLOW_PHASE_TITLE_MAX_BYTES + 1,
            },
        ),
        (
            blank_phase,
            WorkflowModelError::EmptyText {
                field: "phase title",
            },
        ),
        (
            too_many_phases,
            WorkflowModelError::TooManyDeclaredPhases {
                maximum: phase_limit,
                actual: phase_limit + 1,
            },
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(WorkflowRunModel::from_event(&event), Err(expected));
    }
}

#[test]
fn run_begin_enforces_projection_owned_text_bounds() {
    let mut exact = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut exact else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.run_id = "r".repeat(WORKFLOW_RUN_ID_MAX_BYTES);
    event.resumed_from_run_id = Some("p".repeat(WORKFLOW_RUN_ID_MAX_BYTES));
    event.args_digest = "d".repeat(WORKFLOW_ARGS_DIGEST_MAX_BYTES);
    assert!(WorkflowRunModel::from_event(&exact).is_ok());

    let mut long_run_id = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut long_run_id else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.run_id = "r".repeat(WORKFLOW_RUN_ID_MAX_BYTES + 1);

    let mut empty_resume_id = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut empty_resume_id else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.resumed_from_run_id = Some(String::new());

    let mut long_resume_id = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut long_resume_id else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.resumed_from_run_id = Some("r".repeat(WORKFLOW_RUN_ID_MAX_BYTES + 1));

    let mut empty_args_digest = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut empty_args_digest else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.args_digest.clear();

    let mut long_args_digest = run_begin(&["plan"]);
    let WorkflowEvent::RunBegin(event) = &mut long_args_digest else {
        unreachable!("run_begin helper returns run_begin")
    };
    event.args_digest = "d".repeat(WORKFLOW_ARGS_DIGEST_MAX_BYTES + 1);

    let cases = [
        (
            long_run_id,
            WorkflowModelError::TextTooLong {
                field: "run_id",
                maximum_bytes: WORKFLOW_RUN_ID_MAX_BYTES,
                actual_bytes: WORKFLOW_RUN_ID_MAX_BYTES + 1,
            },
        ),
        (
            empty_resume_id,
            WorkflowModelError::EmptyText {
                field: "resumed_from_run_id",
            },
        ),
        (
            long_resume_id,
            WorkflowModelError::TextTooLong {
                field: "resumed_from_run_id",
                maximum_bytes: WORKFLOW_RUN_ID_MAX_BYTES,
                actual_bytes: WORKFLOW_RUN_ID_MAX_BYTES + 1,
            },
        ),
        (
            empty_args_digest,
            WorkflowModelError::EmptyText {
                field: "args_digest",
            },
        ),
        (
            long_args_digest,
            WorkflowModelError::TextTooLong {
                field: "args_digest",
                maximum_bytes: WORKFLOW_ARGS_DIGEST_MAX_BYTES,
                actual_bytes: WORKFLOW_ARGS_DIGEST_MAX_BYTES + 1,
            },
        ),
    ];

    for (event, expected) in cases {
        assert_eq!(WorkflowRunModel::from_event(&event), Err(expected));
    }
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

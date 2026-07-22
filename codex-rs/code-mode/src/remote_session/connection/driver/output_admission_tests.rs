use codex_code_mode_protocol::CellId;
use codex_code_mode_protocol::ExecuteOutputPolicy;
use codex_code_mode_protocol::FunctionCallOutputContentItem;
use codex_code_mode_protocol::RuntimeResponse;
use codex_code_mode_protocol::SAVED_WORKFLOW_EXECUTION_FAILED;
use codex_code_mode_protocol::SAVED_WORKFLOW_OUTPUT_REJECTED;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_ITEM_MAX_BYTES;
use codex_code_mode_protocol::WORKFLOW_OUTPUT_MAX_BYTES;
use pretty_assertions::assert_eq;

use super::AdmissionOutcome;
use super::MAX_UNMATCHED_TERMINAL_ECHOES;
use super::RemoteOutputAdmission;
use super::ResponseDelivery;
use super::TerminalEchoBudget;

fn response(cell: &str, text: String) -> RuntimeResponse {
    RuntimeResponse::Yielded {
        cell_id: CellId::new(cell.to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText { text }],
    }
}

fn terminal_response(cell: &str, error_text: Option<&str>) -> RuntimeResponse {
    RuntimeResponse::Result {
        cell_id: CellId::new(cell.to_string()),
        content_items: Vec::new(),
        error_text: error_text.map(str::to_string),
    }
}

fn admit(
    admission: &RemoteOutputAdmission,
    response: &RuntimeResponse,
    delivery: ResponseDelivery,
) -> (AdmissionOutcome, RuntimeResponse) {
    let mut visible = response.clone();
    let outcome = admission.admit_response(&mut visible, delivery);
    (outcome, visible)
}

#[test]
fn ordinary_output_is_preserved_above_saved_limits() {
    let admission = RemoteOutputAdmission::new(ExecuteOutputPolicy::Ordinary);
    let oversized = "x".repeat(WORKFLOW_OUTPUT_MAX_BYTES + 1);
    let mut actual = response("ordinary", oversized.clone());
    let expected = actual.clone();
    assert_eq!(
        admission.admit_response(&mut actual, ResponseDelivery::Uncorrelated),
        AdmissionOutcome::Admitted
    );
    assert_eq!(actual, expected);
    assert_eq!(
        admission.admit_error(oversized.clone()),
        (oversized.clone(), AdmissionOutcome::Admitted)
    );
    assert_eq!(
        admission.admit_fatal_error(oversized.clone()),
        (oversized.clone(), AdmissionOutcome::Admitted)
    );
    assert_eq!(
        admission.visible_connection_failure(oversized.clone()),
        oversized
    );
}

#[test]
fn saved_clones_share_one_cumulative_chunk_ledger() {
    let first = RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow);
    let second = first.clone();
    let admissions = [&first, &second];
    let chunk = "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 2);
    for index in 0..WORKFLOW_OUTPUT_MAX_BYTES / WORKFLOW_OUTPUT_ITEM_MAX_BYTES {
        let mut actual = response(&index.to_string(), chunk.clone());
        assert_eq!(
            admissions[index % admissions.len()]
                .admit_response(&mut actual, ResponseDelivery::Observer),
            AdmissionOutcome::Admitted
        );
    }
    assert_eq!(
        second.admit_fatal_error(String::new()),
        (
            SAVED_WORKFLOW_OUTPUT_REJECTED.to_string(),
            AdmissionOutcome::Rejected
        )
    );
    let mut overflow = response("overflow", String::new());
    assert_eq!(
        first.admit_response(&mut overflow, ResponseDelivery::Observer),
        AdmissionOutcome::Rejected
    );
    assert_eq!(
        overflow,
        RuntimeResponse::Result {
            cell_id: CellId::new("overflow".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string()),
        }
    );
}

#[test]
fn saved_errors_are_redacted_bounded_and_sticky() {
    let admission = RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow);
    let mut terminal = RuntimeResponse::Result {
        cell_id: CellId::new("terminal".to_string()),
        content_items: Vec::new(),
        error_text: Some("private terminal detail".to_string()),
    };
    assert_eq!(
        admission.admit_response(&mut terminal, ResponseDelivery::Observer),
        AdmissionOutcome::Admitted
    );
    assert_eq!(
        terminal,
        RuntimeResponse::Result {
            cell_id: CellId::new("terminal".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
        }
    );
    assert_eq!(
        admission.admit_error("private transport detail".to_string()),
        (
            SAVED_WORKFLOW_EXECUTION_FAILED.to_string(),
            AdmissionOutcome::Admitted
        )
    );
    let mut continued = response("continued", "ok".to_string());
    let expected = continued.clone();
    assert_eq!(
        admission.admit_response(&mut continued, ResponseDelivery::Observer),
        AdmissionOutcome::Admitted
    );
    assert_eq!(continued, expected);
    let oversized = "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES);
    assert_eq!(
        admission.admit_error(oversized),
        (
            SAVED_WORKFLOW_OUTPUT_REJECTED.to_string(),
            AdmissionOutcome::Rejected
        )
    );
    assert_eq!(
        admission.admit_error("later detail".to_string()),
        (
            SAVED_WORKFLOW_OUTPUT_REJECTED.to_string(),
            AdmissionOutcome::Rejected
        )
    );
}

#[test]
fn saved_fatal_error_is_accounted_once_and_shared_as_fixed_failure() {
    let admission = RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow);
    let sibling = admission.clone();
    let full_item = || FunctionCallOutputContentItem::InputText {
        text: "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 2),
    };
    let mut near_full = RuntimeResponse::Yielded {
        cell_id: CellId::new("fatal".to_string()),
        content_items: vec![
            full_item(),
            full_item(),
            full_item(),
            FunctionCallOutputContentItem::InputText {
                text: "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 5),
            },
        ],
    };
    assert_eq!(
        admission.admit_response(&mut near_full, ResponseDelivery::Observer),
        AdmissionOutcome::Admitted
    );
    assert_eq!(
        sibling.visible_connection_failure("private global failure".to_string()),
        SAVED_WORKFLOW_EXECUTION_FAILED
    );
    let fixed_failure = (
        SAVED_WORKFLOW_EXECUTION_FAILED.to_string(),
        AdmissionOutcome::ExecutionFailed,
    );
    assert_eq!(admission.admit_fatal_error("x".to_string()), fixed_failure);
    assert_eq!(
        sibling.admit_fatal_error("x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES)),
        fixed_failure
    );

    let mut late = response("late", "must not escape".to_string());
    assert_eq!(
        sibling.admit_response(&mut late, ResponseDelivery::Observer),
        AdmissionOutcome::ExecutionFailed
    );
    assert_eq!(
        late,
        RuntimeResponse::Result {
            cell_id: CellId::new("late".to_string()),
            content_items: Vec::new(),
            error_text: Some(SAVED_WORKFLOW_EXECUTION_FAILED.to_string()),
        }
    );
}

#[test]
fn saved_terminal_echo_is_admitted_once_in_both_orders() {
    let full_item = || FunctionCallOutputContentItem::InputText {
        text: "x".repeat(WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 2),
    };
    let raw = RuntimeResponse::Terminated {
        cell_id: CellId::new("terminal".to_string()),
        content_items: vec![
            full_item();
            WORKFLOW_OUTPUT_MAX_BYTES / WORKFLOW_OUTPUT_ITEM_MAX_BYTES
        ],
    };
    for roles in [
        [ResponseDelivery::Observer, ResponseDelivery::Terminate],
        [ResponseDelivery::Terminate, ResponseDelivery::Observer],
    ] {
        let admission = RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow);
        for role in roles {
            assert_eq!(
                admit(&admission, &raw, role),
                (AdmissionOutcome::Admitted, raw.clone())
            );
        }
    }
}

#[test]
fn saved_terminal_echo_compares_raw_errors_before_redaction() {
    let admission = RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow);
    let raw = terminal_response("terminal", Some("private detail"));
    let visible = terminal_response("terminal", Some(SAVED_WORKFLOW_EXECUTION_FAILED));
    assert_eq!(
        [ResponseDelivery::Observer, ResponseDelivery::Terminate]
            .map(|role| admit(&admission, &raw, role)),
        [
            (AdmissionOutcome::Admitted, visible.clone()),
            (AdmissionOutcome::Admitted, visible),
        ]
    );
    let distinct = terminal_response("terminal", Some("different private detail"));
    for (second, role) in [
        (&raw, ResponseDelivery::Observer),
        (&distinct, ResponseDelivery::Terminate),
    ] {
        let mismatched = RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow);
        let outcomes = [
            admit(&mismatched, &raw, ResponseDelivery::Observer).0,
            admit(&mismatched, second, role).0,
        ];
        let expected = [
            AdmissionOutcome::Admitted,
            AdmissionOutcome::ExecutionFailed,
        ];
        assert_eq!(outcomes, expected);
    }
}

#[test]
fn saved_late_yield_is_admitted_without_claiming_a_terminal_role() {
    let admission = RemoteOutputAdmission::new(ExecuteOutputPolicy::SavedWorkflow);
    let raw = terminal_response("terminal", /*error_text*/ None);
    let late = response("terminal", "late output".to_string());
    assert_eq!(
        [
            admit(&admission, &raw, ResponseDelivery::Observer).0,
            admit(&admission, &late, ResponseDelivery::Observer).0,
            admit(&admission, &raw, ResponseDelivery::Terminate).0,
        ],
        [AdmissionOutcome::Admitted; 3]
    );
}

#[test]
fn saved_terminal_snapshot_budget_is_released_on_match_drop_and_failure() {
    let budget = TerminalEchoBudget::new();
    let mut admissions = Vec::new();
    let raw = terminal_response("terminal", /*error_text*/ None);
    for _ in 0..MAX_UNMATCHED_TERMINAL_ECHOES {
        let admission = RemoteOutputAdmission::with_terminal_echo_budget(
            ExecuteOutputPolicy::SavedWorkflow,
            budget.clone(),
        );
        assert_eq!(
            admit(&admission, &raw, ResponseDelivery::Observer).0,
            AdmissionOutcome::Admitted
        );
        admissions.push(admission);
    }

    let rejected = RemoteOutputAdmission::with_terminal_echo_budget(
        ExecuteOutputPolicy::SavedWorkflow,
        budget.clone(),
    );
    assert_eq!(
        admit(&rejected, &raw, ResponseDelivery::Observer).0,
        AdmissionOutcome::ExecutionFailed
    );

    let released = admit(&admissions[0], &raw, ResponseDelivery::Terminate).0;
    assert_eq!(released, AdmissionOutcome::Admitted);
    let replacement = RemoteOutputAdmission::with_terminal_echo_budget(
        ExecuteOutputPolicy::SavedWorkflow,
        budget.clone(),
    );
    assert_eq!(
        admit(&replacement, &raw, ResponseDelivery::Observer).0,
        AdmissionOutcome::Admitted
    );

    drop(admissions.pop());
    let drop_replacement = RemoteOutputAdmission::with_terminal_echo_budget(
        ExecuteOutputPolicy::SavedWorkflow,
        budget.clone(),
    );
    assert_eq!(
        admit(&drop_replacement, &raw, ResponseDelivery::Observer).0,
        AdmissionOutcome::Admitted
    );

    assert_eq!(
        admit(&admissions[1], &raw, ResponseDelivery::Observer).0,
        AdmissionOutcome::ExecutionFailed
    );
    let failure_replacement = RemoteOutputAdmission::with_terminal_echo_budget(
        ExecuteOutputPolicy::SavedWorkflow,
        budget,
    );
    assert_eq!(
        admit(&failure_replacement, &raw, ResponseDelivery::Observer).0,
        AdmissionOutcome::Admitted
    );
}

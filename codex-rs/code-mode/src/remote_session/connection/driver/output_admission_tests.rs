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
use super::RemoteOutputAdmission;

fn response(cell: &str, text: String) -> RuntimeResponse {
    RuntimeResponse::Yielded {
        cell_id: CellId::new(cell.to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText { text }],
    }
}

#[test]
fn ordinary_output_is_preserved_above_saved_limits() {
    let admission = RemoteOutputAdmission::new(ExecuteOutputPolicy::Ordinary);
    let oversized = "x".repeat(WORKFLOW_OUTPUT_MAX_BYTES + 1);
    let mut actual = response("ordinary", oversized.clone());
    let expected = actual.clone();
    assert_eq!(
        admission.admit_response(&mut actual),
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
            admissions[index % admissions.len()].admit_response(&mut actual),
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
        first.admit_response(&mut overflow),
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
        admission.admit_response(&mut terminal),
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
        admission.admit_response(&mut continued),
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
        admission.admit_response(&mut near_full),
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
        sibling.admit_response(&mut late),
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

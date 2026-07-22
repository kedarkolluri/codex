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
        (oversized, AdmissionOutcome::Admitted)
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
    let mut overflow = response("overflow", String::new());
    assert_eq!(
        second.admit_response(&mut overflow),
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

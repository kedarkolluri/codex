use codex_code_mode::AgentSpawnOutcome;
use codex_code_mode::CellId;
use codex_code_mode::FunctionCallOutputContentItem;
use codex_code_mode::RuntimeResponse;
use pretty_assertions::assert_eq;

use super::admit_nested_workflow_depth;
use super::join_result_text;
use super::workflow_result_outcome;

#[test]
fn workflow_result_outcome_returns_joined_text_on_success() {
    let response = RuntimeResponse::Result {
        cell_id: CellId::new("2".to_string()),
        content_items: vec![
            FunctionCallOutputContentItem::InputText {
                text: "child-result".to_string(),
            },
            FunctionCallOutputContentItem::InputText {
                text: "line-2".to_string(),
            },
        ],
        error_text: None,
    };
    match workflow_result_outcome(response) {
        AgentSpawnOutcome::Completed(serde_json::Value::String(text)) => {
            assert_eq!(text, "child-result\nline-2");
        }
        other => panic!("expected Completed(String), got {other:?}"),
    }
}

#[test]
fn workflow_result_outcome_empty_result_is_failed() {
    let response = RuntimeResponse::Result {
        cell_id: CellId::new("2".to_string()),
        content_items: Vec::new(),
        error_text: None,
    };
    assert!(matches!(
        workflow_result_outcome(response),
        AgentSpawnOutcome::Failed
    ));
}

#[test]
fn workflow_result_outcome_terminated_discards_partial_text() {
    let response = RuntimeResponse::Terminated {
        cell_id: CellId::new("2".to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "partial-before-death".to_string(),
        }],
    };
    assert!(matches!(
        workflow_result_outcome(response),
        AgentSpawnOutcome::Failed
    ));
}

#[test]
fn workflow_runtime_and_durable_return_caps_stay_aligned() {
    assert_eq!(
        codex_code_mode::WORKFLOW_OUTPUT_MAX_BYTES,
        codex_workflow_journal::WORKFLOW_AGENT_RETURN_MAX_BYTES
    );
}

#[test]
fn workflow_result_outcome_rejects_oversized_nested_return() {
    let response = RuntimeResponse::Result {
        cell_id: CellId::new("2".to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "x".repeat(codex_workflow_journal::WORKFLOW_AGENT_RETURN_MAX_BYTES),
        }],
        error_text: None,
    };
    match workflow_result_outcome(response) {
        AgentSpawnOutcome::Rejected(message) => {
            assert!(message.contains("nested workflow result rejected"));
            assert!(message.contains("replay cap"));
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn workflow_result_outcome_script_error_is_rejected() {
    let response = RuntimeResponse::Result {
        cell_id: CellId::new("2".to_string()),
        content_items: Vec::new(),
        error_text: Some("boom in child".to_string()),
    };
    match workflow_result_outcome(response) {
        AgentSpawnOutcome::Rejected(message) => {
            assert!(message.contains("nested workflow run failed"));
            assert!(message.contains("boom in child"));
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[test]
fn join_result_text_filters_blank_and_joins() {
    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: "a".to_string(),
        },
        FunctionCallOutputContentItem::InputText {
            text: "   ".to_string(),
        },
        FunctionCallOutputContentItem::InputText {
            text: "b".to_string(),
        },
    ];
    assert_eq!(join_result_text(&items), Some("a\nb".to_string()));
    assert_eq!(join_result_text(&[]), None);
}

#[test]
fn nested_workflow_depth_admits_one_level_and_rejects_two() {
    assert_eq!(admit_nested_workflow_depth(0), Ok(1));
    assert_eq!(admit_nested_workflow_depth(1), Err(2));
}

#[test]
fn nested_workflow_depth_is_always_capped_at_one_level() {
    assert_eq!(admit_nested_workflow_depth(1), Err(2));
    assert_eq!(admit_nested_workflow_depth(2), Err(3));
    assert_eq!(admit_nested_workflow_depth(0), Ok(1));
}

#[test]
fn workflow_result_outcome_yielded_is_rejected_not_completed() {
    let response = RuntimeResponse::Yielded {
        cell_id: CellId::new("2".to_string()),
        content_items: vec![FunctionCallOutputContentItem::InputText {
            text: "partial".to_string(),
        }],
    };
    assert!(matches!(
        workflow_result_outcome(response),
        AgentSpawnOutcome::Rejected(_)
    ));
}

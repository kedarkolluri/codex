use std::sync::Arc;

use codex_core_workflows::WorkflowBudget;
use codex_core_workflows::WorkflowBudgetLimit;

use super::ERROR_TRUNCATION_MARKER;
use super::MAX_MODEL_ERROR_BYTES;
use super::WorkflowBudgetSpec;
use super::bound_model_error;
use super::protocol_budget_snapshot;
use super::truncate_model_error;
use super::workflow_budget_limit;
use super::workflow_budget_spec;
use crate::function_tool::FunctionCallError;

#[test]
fn workflow_budget_spec_reads_positive_total_from_args() {
    let args = serde_json::json!({ "budget": { "total": 500_000 }, "input": "x" });
    assert_eq!(
        workflow_budget_spec(&args),
        WorkflowBudgetSpec::Limit(500_000)
    );
}

#[test]
fn workflow_budget_spec_absent_for_null_or_missing_budget() {
    assert_eq!(
        workflow_budget_spec(&serde_json::Value::Null),
        WorkflowBudgetSpec::Absent
    );
    assert_eq!(
        workflow_budget_spec(&serde_json::json!({})),
        WorkflowBudgetSpec::Absent
    );
    assert_eq!(
        workflow_budget_spec(&serde_json::json!({ "budget": {} })),
        WorkflowBudgetSpec::Absent
    );
    assert_eq!(
        workflow_budget_spec(&serde_json::json!({ "budget": { "total": "500" } })),
        WorkflowBudgetSpec::Absent
    );
}

#[test]
fn workflow_budget_spec_zero_is_a_real_ceiling_and_negatives_clamp() {
    assert_eq!(
        workflow_budget_spec(&serde_json::json!({ "budget": { "total": 0 } })),
        WorkflowBudgetSpec::Limit(0)
    );
    assert_eq!(
        workflow_budget_spec(&serde_json::json!({ "budget": { "total": -1 } })),
        WorkflowBudgetSpec::Limit(0)
    );
}

#[test]
fn workflow_budget_limit_is_run_local_and_preserves_zero() {
    assert_eq!(
        workflow_budget_limit(&serde_json::json!({})),
        WorkflowBudgetLimit::Unmetered
    );
    assert_eq!(
        workflow_budget_limit(&serde_json::json!({ "budget": { "total": 100 } })),
        WorkflowBudgetLimit::Limited(100)
    );
    assert_eq!(
        workflow_budget_limit(&serde_json::json!({ "budget": { "total": 0 } })),
        WorkflowBudgetLimit::Limited(0)
    );
}

#[test]
fn nested_budget_inherits_and_tightens_without_sibling_leakage() {
    let parent = WorkflowBudget::new(WorkflowBudgetLimit::Limited(100));
    let inherited = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Unmetered);
    let tight = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Limited(20));
    let loose = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Limited(1_000));

    tight.record_spent(8);

    assert_eq!(protocol_budget_snapshot(&tight).total, Some(20));
    assert_eq!(protocol_budget_snapshot(&tight).spent, 8);
    assert_eq!(protocol_budget_snapshot(&inherited).spent, 8);
    assert_eq!(protocol_budget_snapshot(&loose).total, Some(92));
    assert_eq!(loose.snapshot().spent, 0);
}

#[test]
fn short_error_is_unchanged() {
    let message = "invalid workflow `meta` manifest: boom".to_string();
    assert_eq!(truncate_model_error(message.clone()), message);
}

#[test]
fn oversized_error_is_hard_truncated_with_marker() {
    let message = "z".repeat(MAX_MODEL_ERROR_BYTES * 4);
    let truncated = truncate_model_error(message);
    assert!(truncated.ends_with(ERROR_TRUNCATION_MARKER));
    assert!(truncated.len() <= MAX_MODEL_ERROR_BYTES);
}

#[test]
fn truncation_respects_utf8_boundaries() {
    let message = "€".repeat(MAX_MODEL_ERROR_BYTES);
    let truncated = truncate_model_error(message);
    assert!(truncated.len() <= MAX_MODEL_ERROR_BYTES);
    assert!(truncated.ends_with(ERROR_TRUNCATION_MARKER));
}

#[test]
fn bound_model_error_preserves_variant_and_truncates() {
    let long = "y".repeat(MAX_MODEL_ERROR_BYTES * 2);
    match bound_model_error(FunctionCallError::RespondToModel(long.clone())) {
        FunctionCallError::RespondToModel(message) => {
            assert!(message.ends_with(ERROR_TRUNCATION_MARKER));
            assert!(message.len() < long.len());
        }
        other => panic!("expected RespondToModel, got {other:?}"),
    }
    match bound_model_error(FunctionCallError::Fatal(long)) {
        FunctionCallError::Fatal(message) => {
            assert!(message.ends_with(ERROR_TRUNCATION_MARKER));
        }
        other => panic!("expected Fatal, got {other:?}"),
    }
}

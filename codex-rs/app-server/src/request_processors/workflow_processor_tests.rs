use super::WORKFLOW_START_MAX_ARGS_BYTES;
use super::WORKFLOW_START_MAX_NAME_BYTES;
use super::validate_workflow_args;
use super::validate_workflow_name;
use pretty_assertions::assert_eq;

#[test]
fn workflow_start_input_bounds_are_enforced() {
    validate_workflow_name("release-audit").expect("ordinary name should be accepted");
    validate_workflow_args(&serde_json::json!({"target": "main"}))
        .expect("ordinary args should be accepted");

    let empty = validate_workflow_name("  ").expect_err("empty name should be rejected");
    assert_eq!(empty.message, "workflow name must not be empty");

    let oversized_name = "n".repeat(WORKFLOW_START_MAX_NAME_BYTES + 1);
    let error =
        validate_workflow_name(&oversized_name).expect_err("oversized name should be rejected");
    assert_eq!(
        error.message,
        format!("workflow name exceeds the {WORKFLOW_START_MAX_NAME_BYTES}-byte limit")
    );

    let oversized_args = serde_json::Value::String("a".repeat(WORKFLOW_START_MAX_ARGS_BYTES));
    let error = validate_workflow_args(&oversized_args)
        .expect_err("serialized args over the cap should be rejected");
    assert_eq!(
        error.message,
        format!("workflow args exceed the {WORKFLOW_START_MAX_ARGS_BYTES}-byte execution cap")
    );
}

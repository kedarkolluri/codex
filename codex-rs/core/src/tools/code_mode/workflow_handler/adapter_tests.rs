use super::WorkflowRunArgs;
use super::parse_workflow_run_args;
use crate::function_tool::FunctionCallError;

#[test]
fn model_workflow_arguments_reject_inline_source_and_paths() {
    for forbidden in ["source", "script", "path", "scriptPath"] {
        let mut arguments = serde_json::json!({"name": "saved-workflow"});
        arguments[forbidden] = serde_json::json!("export const meta = {};");
        let error = serde_json::from_value::<WorkflowRunArgs>(arguments)
            .expect_err("raw source and path fields must be rejected");
        assert!(error.to_string().contains("unknown field"));
    }
}

#[test]
fn malformed_workflow_arguments_are_bounded_during_parsing() {
    let field = "x".repeat(3000);
    let arguments = format!(r#"{{"{field}":true}}"#);
    let error = parse_workflow_run_args(&arguments)
        .expect_err("unknown workflow argument must be rejected");
    let FunctionCallError::RespondToModel(message) = error else {
        panic!("expected model-visible parse error, got {error:?}");
    };
    assert!(message.len() <= 2048);
    assert!(message.ends_with("… [error truncated]"));
}

#[test]
fn oversized_raw_model_workflow_call_is_rejected() {
    let arguments = format!(
        r#"{{"name":"demo","args":{{"padding":"{}"}}}}"#,
        "x".repeat(codex_code_mode::WORKFLOW_MODEL_CALL_MAX_BYTES)
    );
    let error = parse_workflow_run_args(&arguments)
        .expect_err("oversized model-authored workflow call should fail");

    assert_eq!(
        error,
        FunctionCallError::RespondToModel(format!(
            "{} arguments exceed the {}-byte model-context cap",
            super::WORKFLOW_TOOL_NAME,
            codex_code_mode::WORKFLOW_MODEL_CALL_MAX_BYTES
        ))
    );
}

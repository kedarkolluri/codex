use serde_json::Value;

use super::ExecContext;
use super::resume_saved_workflow_with_persisted_invocation_for_turn;
use super::start_saved_workflow_for_turn;
use crate::function_tool::FunctionCallError;
use crate::session::tests::make_workflow_session_and_context_with_rx;

#[tokio::test]
async fn workflow_managed_child_cannot_enter_top_level_workflow_handler() {
    let (session, _original_turn, _events) = make_workflow_session_and_context_with_rx().await;
    let turn = session.new_default_turn().await;
    let exec = ExecContext { session, turn };

    let start_error = start_saved_workflow_for_turn(
        exec.clone(),
        "forged-workflow-start".to_string(),
        Vec::new(),
        "must-not-resolve",
        Value::Null,
        /*resume_from_run_id*/ None,
    )
    .await
    .expect_err("workflow child handler start must fail closed");
    let resume_error = resume_saved_workflow_with_persisted_invocation_for_turn(
        exec,
        "forged-workflow-resume".to_string(),
        Vec::new(),
        "00000000-0000-0000-0000-000000000000",
    )
    .await
    .expect_err("workflow child handler resume must fail closed");

    for error in [start_error, resume_error] {
        let FunctionCallError::RespondToModel(message) = error else {
            panic!("workflow child handler returned an unexpected error");
        };
        assert_eq!(
            message,
            "workflow_run is unavailable in workflow-managed threads"
        );
    }
}

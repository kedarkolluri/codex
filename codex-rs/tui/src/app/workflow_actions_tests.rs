use super::*;
use crate::app_event::WorkflowSaveRunTarget;
use crate::app_event::WorkflowSaveTarget;
use codex_app_server_protocol::WorkflowSaveScope;
use pretty_assertions::assert_eq;

#[test]
fn workflow_save_params_preserve_typed_target_and_intent() {
    let thread_id = ThreadId::new();
    let target = WorkflowSaveTarget {
        run: WorkflowSaveRunTarget {
            thread_id,
            run_id: "run-save".to_string(),
            name: "release-check".to_string(),
        },
        scope: WorkflowSaveScope::Project,
    };

    for (intent, overwrite) in [
        (WorkflowSaveIntent::Create, false),
        (WorkflowSaveIntent::Overwrite, true),
    ] {
        assert_eq!(
            workflow_save_params(&WorkflowSaveRequest {
                target: target.clone(),
                intent,
            }),
            WorkflowSaveParams {
                thread_id: thread_id.to_string(),
                run_id: "run-save".to_string(),
                name: "release-check".to_string(),
                scope: WorkflowSaveScope::Project,
                overwrite,
            }
        );
    }
}

#[tokio::test]
async fn workflow_stop_uses_embedded_request_path_and_surfaces_server_errors() -> Result<()> {
    let app = crate::app::test_support::make_test_app().await;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let thread = app_server.start_thread(&app.config).await?;

    let error = send_workflow_stop(
        app_server.request_handle(),
        WorkflowStopParams {
            thread_id: thread.session.thread_id.to_string(),
            run_id: Uuid::new_v4().to_string(),
        },
    )
    .await
    .expect_err("the disabled workflow API must reject the request");

    let error_chain = error.chain().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        error_chain.first().map(String::as_str),
        Some("workflow/stop failed in TUI")
    );
    assert!(
        error_chain
            .iter()
            .any(|cause| cause.contains("workflow feature is disabled for thread")),
        "unexpected stop error chain: {error_chain:#?}"
    );
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn workflow_save_uses_embedded_request_path_and_surfaces_server_errors() -> Result<()> {
    let app = crate::app::test_support::make_test_app().await;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let thread = app_server.start_thread(&app.config).await?;

    let error = send_workflow_save(
        app_server.request_handle(),
        WorkflowSaveParams {
            thread_id: thread.session.thread_id.to_string(),
            run_id: Uuid::new_v4().to_string(),
            name: "release-check".to_string(),
            scope: WorkflowSaveScope::Personal,
            overwrite: false,
        },
    )
    .await
    .expect_err("the disabled workflow API must reject the request");

    let error_chain = error.chain().map(ToString::to_string).collect::<Vec<_>>();
    assert_eq!(
        error_chain.first().map(String::as_str),
        Some("workflow/save failed in TUI")
    );
    assert!(
        error_chain
            .iter()
            .any(|cause| cause.contains("workflow feature is disabled for thread")),
        "unexpected save error chain: {error_chain:#?}"
    );
    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn workflow_control_requests_use_typed_embedded_paths() -> Result<()> {
    let app = crate::app::test_support::make_test_app().await;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let thread = app_server.start_thread(&app.config).await?;
    let thread_id = thread.session.thread_id.to_string();
    let run_id = Uuid::new_v4().to_string();

    let pause_error = send_workflow_pause(
        app_server.request_handle(),
        WorkflowPauseParams {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        },
    )
    .await
    .expect_err("an unavailable workflow cannot be paused");
    assert_eq!(
        pause_error.chain().next().map(ToString::to_string),
        Some("workflow/pause failed in TUI".to_string())
    );

    let resume_error = send_workflow_resume(
        app_server.request_handle(),
        WorkflowResumeParams {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
        },
    )
    .await
    .expect_err("an unavailable workflow cannot be resumed");
    assert_eq!(
        resume_error.chain().next().map(ToString::to_string),
        Some("workflow/resume failed in TUI".to_string())
    );

    let agent_error = send_workflow_agent_control(
        app_server.request_handle(),
        WorkflowAgentControlParams {
            thread_id,
            run_id,
            node_id: 7,
            attempt: 0,
            action: codex_app_server_protocol::WorkflowAgentControlAction::Retry,
        },
    )
    .await
    .expect_err("an unavailable workflow agent cannot be retried");
    assert_eq!(
        agent_error.chain().next().map(ToString::to_string),
        Some("workflow/agent/control failed in TUI".to_string())
    );

    app_server.shutdown().await?;
    Ok(())
}

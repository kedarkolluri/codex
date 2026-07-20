use super::*;

#[tokio::test]
async fn workflow_inline_dispatch_emits_exact_start_app_event() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.set_feature_enabled(Feature::Workflow, /*enabled*/ true);

    chat.dispatch_command_with_args(
        SlashCommand::Workflow,
        r#"release-audit {"target":"main","dryRun":true}"#.to_string(),
        Vec::new(),
    );

    match rx.try_recv() {
        Ok(AppEvent::StartSavedWorkflow {
            thread_id: event_thread_id,
            name,
            args,
        }) => {
            assert_eq!(event_thread_id, thread_id);
            assert_eq!(name, "release-audit");
            assert_eq!(args, Some(json!({"target": "main", "dryRun": true})));
        }
        other => panic!("expected StartSavedWorkflow event, got {other:?}"),
    }
}

#[tokio::test]
async fn workflow_picker_refreshes_during_an_active_task_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.set_feature_enabled(Feature::Workflow, /*enabled*/ true);
    chat.bottom_pane.set_task_running(/*running*/ true);

    chat.dispatch_command(SlashCommand::Workflow);
    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::LoadSavedWorkflows { thread_id: event_thread_id })
            if event_thread_id == thread_id
    );
    chat.on_workflow_list_result(
        thread_id,
        Ok(vec![
            workflow_metadata(
                "release-audit",
                "Audit the release candidate with parallel reviewers.",
                &["Prepare", "Review", "Report"],
                codex_app_server_protocol::WorkflowScope::Project,
            ),
            workflow_metadata(
                "dependency-sweep",
                "Find risky dependency updates and validate the safe set.",
                &["Discover", "Validate"],
                codex_app_server_protocol::WorkflowScope::Personal,
            ),
        ]),
    );

    assert_chatwidget_snapshot!(
        "workflow_picker_loaded",
        render_bottom_popup(&chat, /*width*/ 100)
    );
}

#[tokio::test]
async fn workflow_picker_selection_starts_with_default_args() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.set_feature_enabled(Feature::Workflow, /*enabled*/ true);
    chat.dispatch_command(SlashCommand::Workflow);
    assert_matches!(rx.try_recv(), Ok(AppEvent::LoadSavedWorkflows { .. }));
    chat.on_workflow_list_result(
        thread_id,
        Ok(vec![workflow_metadata(
            "release-audit",
            "Audit the release candidate.",
            &["Review"],
            codex_app_server_protocol::WorkflowScope::Project,
        )]),
    );

    chat.handle_key_event(KeyEvent::from(KeyCode::Enter));

    match rx.try_recv() {
        Ok(AppEvent::StartSavedWorkflow {
            thread_id: event_thread_id,
            name,
            args,
        }) => {
            assert_eq!(event_thread_id, thread_id);
            assert_eq!(name, "release-audit");
            assert_eq!(args, None);
        }
        other => panic!("expected StartSavedWorkflow event, got {other:?}"),
    }
}

#[tokio::test]
async fn workflows_changed_refreshes_only_the_open_picker() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.set_feature_enabled(Feature::Workflow, /*enabled*/ true);
    chat.dispatch_command(SlashCommand::Workflow);
    assert_matches!(rx.try_recv(), Ok(AppEvent::LoadSavedWorkflows { .. }));
    chat.on_workflow_list_result(
        thread_id,
        Ok(vec![workflow_metadata(
            "release-audit",
            "Audit the release candidate.",
            &["Review"],
            codex_app_server_protocol::WorkflowScope::Project,
        )]),
    );

    chat.handle_server_notification(
        ServerNotification::WorkflowsChanged(
            codex_app_server_protocol::WorkflowsChangedNotification::default(),
        ),
        /*replay_kind*/ None,
    );

    assert_matches!(
        rx.try_recv(),
        Ok(AppEvent::LoadSavedWorkflows { thread_id: event_thread_id })
            if event_thread_id == thread_id
    );
    assert!(render_bottom_popup(&chat, /*width*/ 100).contains("Loading saved workflows"));
}

#[tokio::test]
async fn workflow_picker_error_is_bounded_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    chat.set_feature_enabled(Feature::Workflow, /*enabled*/ true);
    chat.dispatch_command(SlashCommand::Workflow);
    assert_matches!(rx.try_recv(), Ok(AppEvent::LoadSavedWorkflows { .. }));
    let long_error = format!("registry unavailable\n{}", "detail ".repeat(120));

    chat.on_workflow_list_result(thread_id, Err(long_error));

    assert_chatwidget_snapshot!(
        "workflow_picker_error",
        render_bottom_popup(&chat, /*width*/ 100)
    );
    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_chatwidget_snapshot!(
        "workflow_picker_error_history",
        lines_to_single_string(&cells[0])
    );
}

#[tokio::test]
async fn workflow_invalid_json_error_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::Workflow, /*enabled*/ true);

    chat.dispatch_command_with_args(
        SlashCommand::Workflow,
        "release-audit {not-json}".to_string(),
        Vec::new(),
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_chatwidget_snapshot!(
        "workflow_invalid_json_error",
        lines_to_single_string(&cells[0])
    );
}

#[tokio::test]
async fn workflow_oversized_json_error_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.set_feature_enabled(Feature::Workflow, /*enabled*/ true);

    chat.dispatch_command_with_args(
        SlashCommand::Workflow,
        format!("release-audit \"{}\"", "x".repeat(32 * 1024)),
        Vec::new(),
    );

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_chatwidget_snapshot!(
        "workflow_oversized_json_error",
        lines_to_single_string(&cells[0])
    );
}

#[tokio::test]
async fn workflow_start_state_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);

    chat.on_workflow_start_requested(thread_id, "release-audit");

    let cells = drain_insert_history(&mut rx);
    assert_eq!(cells.len(), 1);
    assert_chatwidget_snapshot!("workflow_starting", lines_to_single_string(&cells[0]));
}

fn workflow_metadata(
    name: &str,
    description: &str,
    phases: &[&str],
    scope: codex_app_server_protocol::WorkflowScope,
) -> codex_app_server_protocol::WorkflowMetadata {
    codex_app_server_protocol::WorkflowMetadata {
        name: name.to_string(),
        description: description.to_string(),
        phases: phases.iter().map(|phase| (*phase).to_string()).collect(),
        scope,
        path: LegacyAppPathString::from_path(std::path::Path::new("/tmp/workflow.js")),
    }
}

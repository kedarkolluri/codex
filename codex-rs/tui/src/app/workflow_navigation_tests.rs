use super::*;
use codex_app_server_protocol::ServerNotification;
use codex_app_server_protocol::WorkflowAgentBoundNotification;
use codex_app_server_protocol::WorkflowAgentStartedNotification;
use codex_app_server_protocol::WorkflowLogNotification;
use codex_app_server_protocol::WorkflowPhaseChangedNotification;
use codex_app_server_protocol::WorkflowPhaseStatus;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_protocol::openai_models::ReasoningEffort;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn drill_and_return_preserve_parent_monitor_subscription() -> Result<()> {
    let mut app = Box::pin(crate::app::test_support::make_test_app()).await;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let parent = app_server.start_thread(&app.config).await?;
    let child = app_server.start_thread(&app.config).await?;
    let parent_thread_id = parent.session.thread_id;
    let child_thread_id = child.session.thread_id;
    app_server
        .thread_inject_items(child_thread_id, vec![App::side_boundary_prompt_item()])
        .await?;
    app_server.thread_unsubscribe(child_thread_id).await?;
    app.enqueue_primary_thread_session(parent.session, parent.turns)
        .await?;

    let mut tui = crate::tui::test_support::make_test_tui()?;
    for notification in [
        ServerNotification::WorkflowStarted(WorkflowStartedNotification {
            thread_id: parent_thread_id.to_string(),
            run_id: "run-drill-return".to_string(),
            resumed_from_run_id: None,
            name: "drill-return".to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "sha256:drill-return".to_string(),
            started_at: 1,
        }),
        ServerNotification::WorkflowPhaseChanged(WorkflowPhaseChangedNotification {
            thread_id: parent_thread_id.to_string(),
            run_id: "run-drill-return".to_string(),
            phase_index: 0,
            title: "inspect".to_string(),
            status: WorkflowPhaseStatus::Active,
            changed_at: 2,
        }),
        ServerNotification::WorkflowAgentStarted(WorkflowAgentStartedNotification {
            thread_id: parent_thread_id.to_string(),
            run_id: "run-drill-return".to_string(),
            node_id: 7,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: None,
            label: "inspector".to_string(),
            phase: Some("inspect".to_string()),
            model: "gpt-5.5".to_string(),
            effort: ReasoningEffort::High,
            started_at: 3,
        }),
        ServerNotification::WorkflowAgentBound(WorkflowAgentBoundNotification {
            thread_id: parent_thread_id.to_string(),
            run_id: "run-drill-return".to_string(),
            node_id: 7,
            attempt: 0,
            child_thread_id: child_thread_id.to_string(),
            bound_at: 4,
        }),
    ] {
        app.enqueue_thread_notification(parent_thread_id, notification)
            .await?;
    }
    app.drain_active_thread_events(&mut tui).await?;

    app.select_workflow_agent_thread(
        &mut tui,
        &mut app_server,
        child_thread_id,
        "run-drill-return".to_string(),
        /*node_id*/ 7,
    )
    .await?;

    assert_eq!(app.active_thread_id, Some(child_thread_id));
    assert_eq!(app.workflow_monitor_returns.len(), 1);
    assert!(app.thread_event_channels.contains_key(&parent_thread_id));
    app.enqueue_thread_notification(
        parent_thread_id,
        ServerNotification::WorkflowLog(WorkflowLogNotification {
            thread_id: parent_thread_id.to_string(),
            run_id: "run-drill-return".to_string(),
            message: "buffered while inspecting child".to_string(),
            emitted_at: 5,
        }),
    )
    .await?;

    assert!(
        app.maybe_return_to_workflow_monitor(
            &mut tui,
            &mut app_server,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await
    );

    assert_eq!(app.active_thread_id, Some(parent_thread_id));
    assert_eq!(app.workflow_monitor_returns, Vec::new());
    assert!(app.thread_event_channels.contains_key(&parent_thread_id));
    assert!(!app.thread_event_channels.contains_key(&child_thread_id));
    let monitor_text = app.chat_widget.workflow_monitor_text(/*width*/ 80);
    assert!(
        monitor_text.contains("drill-return"),
        "restored workflow monitor:\n{monitor_text}"
    );
    assert!(
        monitor_text.contains("buffered while inspecting child"),
        "restored workflow monitor:\n{monitor_text}"
    );
    assert_eq!(
        app.chat_widget.workflow_monitor_focus(),
        Some(("run-drill-return", 7)),
        "restored workflow monitor:\n{monitor_text}"
    );

    app.enqueue_thread_notification(
        parent_thread_id,
        ServerNotification::WorkflowLog(WorkflowLogNotification {
            thread_id: parent_thread_id.to_string(),
            run_id: "run-drill-return".to_string(),
            message: "parent subscription survived after return".to_string(),
            emitted_at: 6,
        }),
    )
    .await?;
    app.drain_active_thread_events(&mut tui).await?;
    assert!(
        app.chat_widget
            .workflow_monitor_text(/*width*/ 80)
            .contains("parent subscription survived after return")
    );

    app_server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn workflow_monitor_return_stack_eviction_promotes_the_oldest_child() {
    let mut app = crate::app::test_support::make_test_app().await;
    let thread_ids: Vec<_> = (0..=MAX_WORKFLOW_MONITOR_RETURN_TARGETS + 1)
        .map(|_| ThreadId::new())
        .collect();
    for node_id in 0..=MAX_WORKFLOW_MONITOR_RETURN_TARGETS as u64 {
        app.push_workflow_monitor_return(WorkflowMonitorReturnTarget {
            parent_thread_id: thread_ids[node_id as usize],
            child_thread_id: thread_ids[node_id as usize + 1],
            run_id: format!("run-{node_id}"),
            node_id,
            detach_child_on_return: true,
        });
    }

    assert_eq!(
        app.workflow_monitor_returns.len(),
        MAX_WORKFLOW_MONITOR_RETURN_TARGETS
    );
    assert_eq!(
        app.workflow_monitor_returns
            .first()
            .map(|target| target.node_id),
        Some(1)
    );
    assert_eq!(
        app.workflow_monitor_returns
            .first()
            .map(|target| target.parent_thread_id),
        Some(thread_ids[1])
    );
}

#[tokio::test]
async fn drill_return_retains_a_preexisting_child_attachment() -> Result<()> {
    let mut app = Box::pin(crate::app::test_support::make_test_app()).await;
    let mut app_server = Box::pin(crate::start_embedded_app_server_for_picker(&app.config)).await?;
    let parent = app_server.start_thread(&app.config).await?;
    let child = app_server.start_thread(&app.config).await?;
    let parent_thread_id = parent.session.thread_id;
    let child_thread_id = child.session.thread_id;
    app_server
        .thread_inject_items(child_thread_id, vec![App::side_boundary_prompt_item()])
        .await?;
    app_server.thread_unsubscribe(child_thread_id).await?;
    app.enqueue_primary_thread_session(parent.session, parent.turns)
        .await?;
    assert!(
        app.attach_live_thread_for_selection(&mut app_server, child_thread_id)
            .await?
    );

    let mut tui = crate::tui::test_support::make_test_tui()?;
    app.select_workflow_agent_thread(
        &mut tui,
        &mut app_server,
        child_thread_id,
        "run-preexisting-child".to_string(),
        /*node_id*/ 9,
    )
    .await?;

    assert_eq!(app.active_thread_id, Some(child_thread_id));
    assert!(
        app.maybe_return_to_workflow_monitor(
            &mut tui,
            &mut app_server,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        )
        .await
    );

    assert_eq!(app.active_thread_id, Some(parent_thread_id));
    assert!(app.thread_event_channels.contains_key(&child_thread_id));

    app_server.shutdown().await?;
    Ok(())
}

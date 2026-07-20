use super::*;
use crate::app_event::WorkflowSaveIntent;
use crate::app_event::WorkflowSaveRequest;
use crate::app_event::WorkflowSaveRunTarget;
use crate::app_event::WorkflowSaveTarget;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_app_server_protocol::WorkflowAgentBoundNotification;
use codex_app_server_protocol::WorkflowAgentCompletedNotification;
use codex_app_server_protocol::WorkflowAgentStartedNotification;
use codex_app_server_protocol::WorkflowAgentUpdatedNotification;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowGroupCompletedNotification;
use codex_app_server_protocol::WorkflowGroupKind;
use codex_app_server_protocol::WorkflowGroupStartedNotification;
use codex_app_server_protocol::WorkflowLogNotification;
use codex_app_server_protocol::WorkflowPhaseChangedNotification;
use codex_app_server_protocol::WorkflowPhaseStatus;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_app_server_protocol::WorkflowSaveDisposition;
use codex_app_server_protocol::WorkflowSaveScope;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_app_server_protocol::WorkflowStopDisposition;
use codex_config::types::Notifications;
use codex_protocol::openai_models::ReasoningEffort;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;

const THREAD_ID: &str = "00000000-0000-0000-0000-000000000001";
const STATUS_READ_RUN_ID: &str = "019f78be-1234-7abc-8def-0123456789ab";

#[tokio::test]
async fn app_server_notification_updates_monitor_and_requests_redraw() {
    let (mut chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    let thread_id = codex_protocol::ThreadId::new();
    chat.thread_id = Some(thread_id);
    let (draw_tx, mut draw_rx) = tokio::sync::broadcast::channel(/*capacity*/ 4);
    chat.frame_requester = crate::tui::FrameRequester::new(draw_tx);

    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowStarted(
            WorkflowStartedNotification {
                thread_id: thread_id.to_string(),
                run_id: "run-redraw".to_string(),
                resumed_from_run_id: None,
                name: "redraw".to_string(),
                phases: vec!["work".to_string()],
                args_digest: "sha256:redraw".to_string(),
                started_at: 1,
            },
        ),
        /*replay_kind*/ None,
    );

    tokio::time::timeout(std::time::Duration::from_secs(/*secs*/ 1), draw_rx.recv())
        .await
        .expect("workflow update should schedule a frame")
        .expect("draw channel should remain open");
    assert!(chat.workflow_monitor.is_visible());
}

#[tokio::test]
async fn workflow_start_schedules_one_typed_durable_status_read() {
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    let thread_id = workflow_thread_id();
    chat.thread_id = Some(thread_id);
    let started = WorkflowStartedNotification {
        thread_id: thread_id.to_string(),
        run_id: STATUS_READ_RUN_ID.to_string(),
        resumed_from_run_id: None,
        name: "recoverable".to_string(),
        phases: vec!["work".to_string()],
        args_digest: "sha256:recoverable".to_string(),
        started_at: 1,
    };

    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowStarted(started.clone()),
        /*replay_kind*/ None,
    );

    let revision = match event_rx.try_recv().expect("workflow/read request") {
        AppEvent::RequestWorkflowRead {
            thread_id: actual_thread_id,
            run_id,
            revision,
        } if actual_thread_id == thread_id && run_id == STATUS_READ_RUN_ID => revision,
        event => panic!("unexpected app event: {event:?}"),
    };
    chat.workflow_monitor.selected_run_id = Some(STATUS_READ_RUN_ID.to_string());
    assert_eq!(
        (
            chat.workflow_monitor.runs[0].status_read_pending,
            chat.workflow_monitor.runs[0].reconciled_status,
        ),
        (true, Some(WorkflowRunStatus::Unknown))
    );
    assert!(rendered(&chat.workflow_monitor, /*width*/ 100).contains("unknown"));
    assert!(!chat.workflow_monitor.selected_run_can_stop());
    assert!(!chat.workflow_monitor.selected_run_can_pause());

    chat.on_workflow_read_finished(
        codex_protocol::ThreadId::new(),
        STATUS_READ_RUN_ID,
        revision,
        Ok(WorkflowRunStatus::Failed),
    );
    assert!(
        !chat
            .workflow_monitor
            .pending_status_reads
            .contains_key(STATUS_READ_RUN_ID)
    );
    assert_eq!(
        (
            chat.workflow_monitor.runs[0].status_read_pending,
            chat.workflow_monitor.runs[0].reconciled_status,
        ),
        (false, Some(WorkflowRunStatus::Unknown))
    );

    let revision = chat
        .workflow_monitor
        .begin_status_read(STATUS_READ_RUN_ID)
        .expect("replacement status read after wrong-thread result");

    chat.on_workflow_read_finished(
        thread_id,
        STATUS_READ_RUN_ID,
        revision,
        Err("ownerless workflow run is unavailable".to_string()),
    );
    chat.workflow_monitor.selected_run_id = Some(STATUS_READ_RUN_ID.to_string());
    assert!(
        rendered(&chat.workflow_monitor, /*width*/ 100).contains("unknown"),
        "a failed read must fence replayed Running state as unknown"
    );
    assert!(!chat.workflow_monitor.selected_run_can_stop());
    assert!(!chat.workflow_monitor.selected_run_can_pause());

    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowStarted(started),
        /*replay_kind*/ None,
    );
    assert!(
        event_rx.try_recv().is_err(),
        "a duplicate start must not enqueue another status read"
    );
}

#[test]
fn durable_status_override_preserves_topology_and_yields_to_live_events() {
    let mut monitor = WorkflowMonitor::default();
    apply(
        &mut monitor,
        started(STATUS_READ_RUN_ID, "restart-recovery", &["work"]),
    );
    apply(
        &mut monitor,
        phase(
            STATUS_READ_RUN_ID,
            /*phase_index*/ 0,
            "work",
            WorkflowPhaseStatus::Active,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            STATUS_READ_RUN_ID,
            /*node_id*/ 7,
            /*parent_node_id*/ None,
            "worker",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut monitor,
        agent_bound(
            STATUS_READ_RUN_ID,
            /*node_id*/ 7,
            &workflow_thread_id().to_string(),
        ),
    );

    let stale_revision = monitor
        .begin_status_read(STATUS_READ_RUN_ID)
        .expect("initial status read");
    apply(
        &mut monitor,
        agent_updated(
            STATUS_READ_RUN_ID,
            /*node_id*/ 7,
            /*total_tokens*/ 1,
            /*tool_call_count*/ 1,
        ),
    );
    assert!(!monitor.finish_status_read(
        STATUS_READ_RUN_ID,
        stale_revision,
        Some(WorkflowRunStatus::Failed)
    ));
    assert_eq!(monitor.runs[0].reconciled_status, None);
    assert!(
        !monitor
            .pending_status_reads
            .contains_key(STATUS_READ_RUN_ID)
    );

    let topology_before = monitor.runs[0].model.topology.clone();
    monitor.selected_run_id = Some(STATUS_READ_RUN_ID.to_string());

    let revision = monitor
        .begin_status_read(STATUS_READ_RUN_ID)
        .expect("failed status read");
    assert!(!monitor.handle_notification(agent_updated(
        STATUS_READ_RUN_ID,
        /*node_id*/ 99,
        /*total_tokens*/ 1,
        /*tool_call_count*/ 1,
    )));
    assert!(monitor.finish_status_read(
        STATUS_READ_RUN_ID,
        revision,
        Some(WorkflowRunStatus::Failed)
    ));
    assert_eq!(monitor.runs[0].model.topology, topology_before);
    assert_eq!(monitor.runs[0].model.state, WorkflowRunState::Running);
    assert_reconciled_controls(
        &mut monitor,
        /*can_resume*/ false,
        /*can_save*/ false,
    );
    let failed = rendered(&monitor, /*width*/ 100);

    assert_eq!(
        monitor.runs[0].reconciled_status,
        Some(WorkflowRunStatus::Failed),
        "a rejected event must not clear the durable fence"
    );

    apply_reconciled_status(&mut monitor, WorkflowRunStatus::Unknown);
    let unknown = rendered(&monitor, /*width*/ 100);
    assert!(unknown.contains("unknown"));
    assert!(
        !unknown
            .lines()
            .next()
            .is_some_and(|header| header.ends_with("running")),
        "the run header must not remain Running; retained agent topology may still show its last state"
    );
    assert_reconciled_controls(
        &mut monitor,
        /*can_resume*/ false,
        /*can_save*/ false,
    );

    apply_reconciled_status(&mut monitor, WorkflowRunStatus::Stopped);
    assert_reconciled_controls(
        &mut monitor,
        /*can_resume*/ false,
        /*can_save*/ false,
    );
    let stopped = rendered(&monitor, /*width*/ 100);

    apply_reconciled_status(&mut monitor, WorkflowRunStatus::Paused);
    assert_reconciled_controls(
        &mut monitor,
        /*can_resume*/ true,
        /*can_save*/ false,
    );
    let paused = rendered(&monitor, /*width*/ 100);

    apply_reconciled_status(&mut monitor, WorkflowRunStatus::Completed);
    assert_reconciled_controls(
        &mut monitor,
        /*can_resume*/ false,
        /*can_save*/ true,
    );
    let completed_fence = rendered(&monitor, /*width*/ 100);

    let stale_revision = monitor
        .begin_status_read(STATUS_READ_RUN_ID)
        .expect("status read before live event");
    apply(
        &mut monitor,
        agent_updated(
            STATUS_READ_RUN_ID,
            /*node_id*/ 7,
            /*total_tokens*/ 120,
            /*tool_call_count*/ 2,
        ),
    );
    assert!(!monitor.finish_status_read(
        STATUS_READ_RUN_ID,
        stale_revision,
        Some(WorkflowRunStatus::Unknown)
    ));
    assert_eq!(monitor.runs[0].reconciled_status, None);
    monitor.selected_run_id = Some(STATUS_READ_RUN_ID.to_string());
    assert!(monitor.selected_run_can_stop());
    assert!(monitor.selected_run_can_pause());
    let live_again = rendered(&monitor, /*width*/ 100);

    apply(
        &mut monitor,
        agent_completed(
            STATUS_READ_RUN_ID,
            /*node_id*/ 7,
            CollabAgentStatus::Completed,
            Some("done"),
            /*total_tokens*/ 120,
            /*tool_call_count*/ 2,
            /*returned_null*/ false,
        ),
    );
    apply_reconciled_status(&mut monitor, WorkflowRunStatus::Failed);
    let completion_revision = monitor
        .begin_status_read(STATUS_READ_RUN_ID)
        .expect("status read before exact completion");
    apply(
        &mut monitor,
        completed(
            STATUS_READ_RUN_ID,
            CollabAgentStatus::Completed,
            Some("exact completion"),
            /*spent*/ 120,
            /*total*/ Some(500),
        ),
    );
    assert!(
        !monitor
            .pending_status_reads
            .contains_key(STATUS_READ_RUN_ID)
    );
    assert!(!monitor.finish_status_read(
        STATUS_READ_RUN_ID,
        completion_revision,
        Some(WorkflowRunStatus::Unknown)
    ));
    assert_eq!(monitor.runs[0].reconciled_status, None);
    assert_eq!(monitor.runs[0].model.state, WorkflowRunState::Completed);
    let exact_completion = rendered(&monitor, /*width*/ 100);

    insta::assert_snapshot!(
        "workflow_durable_status_override",
        format!(
            "FAILED\n{failed}\n\nUNKNOWN\n{unknown}\n\nSTOPPED\n{stopped}\n\nPAUSED\n{paused}\n\nCOMPLETED FENCE\n{completed_fence}\n\nLIVE AGAIN\n{live_again}\n\nEXACT COMPLETION\n{exact_completion}"
        )
    );
}

#[test]
fn workflow_status_read_concurrency_is_bounded() {
    let mut monitor = WorkflowMonitor::default();
    let run_ids = (0..=MAX_PENDING_WORKFLOW_STATUS_READS)
        .map(|index| format!("019f78be-1234-7abc-8def-{index:012x}"))
        .collect::<Vec<_>>();
    for (index, run_id) in run_ids.iter().enumerate() {
        assert!(monitor.handle_notification(started(
            run_id,
            &format!("bounded {index}"),
            &["work"]
        )));
    }
    let revisions = run_ids
        .iter()
        .take(MAX_PENDING_WORKFLOW_STATUS_READS)
        .map(|run_id| monitor.begin_status_read(run_id).expect("bounded read"))
        .collect::<Vec<_>>();
    assert!(
        monitor
            .begin_status_read(&run_ids[MAX_PENDING_WORKFLOW_STATUS_READS])
            .is_none()
    );
    assert!(!monitor.finish_status_read(&run_ids[0], revisions[0], None));
    assert!(
        monitor
            .begin_status_read(&run_ids[MAX_PENDING_WORKFLOW_STATUS_READS])
            .is_some()
    );
}

#[tokio::test]
async fn completion_notification_uses_monitor_name_and_agent_count() {
    let (mut chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    let thread_id = codex_protocol::ThreadId::new();
    let thread_id_string = thread_id.to_string();
    chat.thread_id = Some(thread_id);
    chat.config.tui_notifications.notifications =
        Notifications::Custom(vec!["workflow-complete".to_string()]);

    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowStarted(
            WorkflowStartedNotification {
                thread_id: thread_id_string.clone(),
                run_id: "run-notification".to_string(),
                resumed_from_run_id: None,
                name: "release train".to_string(),
                phases: Vec::new(),
                args_digest: "sha256:notification".to_string(),
                started_at: 1,
            },
        ),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowAgentStarted(
            WorkflowAgentStartedNotification {
                thread_id: thread_id_string.clone(),
                run_id: "run-notification".to_string(),
                node_id: 1,
                attempt: 0,
                last_attempt_reason: None,
                parent_node_id: None,
                label: "verifier".to_string(),
                phase: None,
                model: "gpt-5.5".to_string(),
                effort: ReasoningEffort::High,
                started_at: 2,
            },
        ),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowAgentBound(
            WorkflowAgentBoundNotification {
                thread_id: thread_id_string.clone(),
                run_id: "run-notification".to_string(),
                node_id: 1,
                attempt: 0,
                child_thread_id: "thread-verifier".to_string(),
                bound_at: 3,
            },
        ),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowAgentCompleted(
            WorkflowAgentCompletedNotification {
                thread_id: thread_id_string.clone(),
                run_id: "run-notification".to_string(),
                node_id: 1,
                attempt: 0,
                last_attempt_reason: None,
                status: CollabAgentStatus::Completed,
                message: Some("verified".to_string()),
                token_usage: usage(/*total_tokens*/ 3_450),
                tool_call_count: 6,
                duration_ms: 0,
                returned_null: false,
                completed_at: 4,
            },
        ),
        /*replay_kind*/ None,
    );
    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowCompleted(
            WorkflowCompletedNotification {
                thread_id: thread_id_string,
                run_id: "run-notification".to_string(),
                status: CollabAgentStatus::Completed,
                message: Some("release ready".to_string()),
                terminal_reason: None,
                spent: 3_600,
                total: Some(8_000),
                completed_at: 5,
            },
        ),
        /*replay_kind*/ None,
    );

    let Some(Notification::WorkflowComplete {
        name,
        status,
        agent_count,
        spent,
    }) = chat.pending_notification.as_ref()
    else {
        panic!("expected a pending workflow completion notification");
    };
    assert_eq!(
        (name.as_str(), status, *agent_count, *spent),
        (
            "release train",
            &CollabAgentStatus::Completed,
            Some(1),
            3_600,
        )
    );
}

#[tokio::test]
async fn summarized_completion_notification_retains_name_without_agent_count() {
    let (mut chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    let thread_id = codex_protocol::ThreadId::new();
    let thread_id_string = thread_id.to_string();
    chat.thread_id = Some(thread_id);
    chat.config.tui_notifications.notifications =
        Notifications::Custom(vec!["workflow-complete".to_string()]);

    for index in 0..=MAX_ACTIVE_RUNS {
        chat.handle_server_notification(
            codex_app_server_protocol::ServerNotification::WorkflowStarted(
                WorkflowStartedNotification {
                    thread_id: thread_id_string.clone(),
                    run_id: format!("run-summary-{index}"),
                    resumed_from_run_id: None,
                    name: format!("summary workflow {index}"),
                    phases: Vec::new(),
                    args_digest: "sha256:summary".to_string(),
                    started_at: 1,
                },
            ),
            /*replay_kind*/ None,
        );
    }
    chat.handle_server_notification(
        codex_app_server_protocol::ServerNotification::WorkflowCompleted(
            WorkflowCompletedNotification {
                thread_id: thread_id_string,
                run_id: format!("run-summary-{MAX_ACTIVE_RUNS}"),
                status: CollabAgentStatus::Completed,
                message: Some("summary complete".to_string()),
                terminal_reason: None,
                spent: 42,
                total: None,
                completed_at: 2,
            },
        ),
        /*replay_kind*/ None,
    );

    let Some(Notification::WorkflowComplete {
        name,
        status,
        agent_count,
        spent,
    }) = chat.pending_notification.as_ref()
    else {
        panic!("expected a summarized workflow completion notification");
    };
    assert_eq!(
        (name.as_str(), status, *agent_count, *spent),
        (
            format!("summary workflow {MAX_ACTIVE_RUNS}").as_str(),
            &CollabAgentStatus::Completed,
            None,
            42,
        )
    );
}

#[test]
fn running_workflow_snapshot() {
    let mut monitor = WorkflowMonitor::default();
    apply(
        &mut monitor,
        started("run-running", "release-check", &["plan", "execute"]),
    );
    apply(
        &mut monitor,
        phase(
            "run-running",
            /*phase_index*/ 0,
            "plan",
            WorkflowPhaseStatus::Active,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-running",
            /*node_id*/ 1,
            /*parent_node_id*/ None,
            "planner",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-running", /*node_id*/ 1, "thread-planner"),
    );
    apply(
        &mut monitor,
        agent_updated(
            "run-running",
            /*node_id*/ 1,
            /*total_tokens*/ 1_240,
            /*tool_call_count*/ 3,
        ),
    );
    for message in [
        "loading inputs",
        "drafting rollout",
        "checking dependencies",
        "ready for review",
    ] {
        apply(&mut monitor, log("run-running", message));
    }

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 80));
}

#[test]
fn bound_child_thread_is_retained_for_drill_snapshot() {
    let mut monitor = active_monitor("run-drill", "drill-ready", "inspect");
    apply(
        &mut monitor,
        agent_started(
            "run-drill",
            /*node_id*/ 9,
            /*parent_node_id*/ None,
            "inspector",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-drill", /*node_id*/ 9, "thread-inspector"),
    );

    let run = monitor.runs.front().expect("monitored workflow run");
    let WorkflowTopologyNode::Agent(agent) = run
        .model
        .topology
        .get(/*key*/ &9)
        .expect("bound workflow agent")
    else {
        panic!("expected agent topology node");
    };
    insta::assert_debug_snapshot!(agent);
}

#[tokio::test]
async fn monitor_keyboard_can_drill_into_a_completed_bound_agent() {
    let completed_thread_id = codex_protocol::ThreadId::new();
    let running_thread_id = codex_protocol::ThreadId::new();
    let mut monitor = active_monitor("run-navigation", "inspectable", "work");
    apply(
        &mut monitor,
        agent_started(
            "run-navigation",
            /*node_id*/ 1,
            /*parent_node_id*/ None,
            "completed-agent",
            "gpt-5.5",
            ReasoningEffort::Medium,
        ),
    );
    apply(
        &mut monitor,
        agent_bound(
            "run-navigation",
            /*node_id*/ 1,
            &completed_thread_id.to_string(),
        ),
    );
    apply(
        &mut monitor,
        agent_completed(
            "run-navigation",
            /*node_id*/ 1,
            CollabAgentStatus::Completed,
            /*message*/ Some("saved"),
            /*total_tokens*/ 320,
            /*tool_call_count*/ 1,
            /*returned_null*/ false,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-navigation",
            /*node_id*/ 2,
            /*parent_node_id*/ None,
            "running-agent",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut monitor,
        agent_bound(
            "run-navigation",
            /*node_id*/ 2,
            &running_thread_id.to_string(),
        ),
    );

    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.workflow_monitor = monitor;

    chat.handle_key_event(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(
        chat.workflow_monitor
            .selection()
            .map(|selection| selection.node_id),
        Some(2),
        "initial focus should prefer the running bound agent"
    );

    chat.handle_key_event(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Up,
        crossterm::event::KeyModifiers::NONE,
    ));
    insta::assert_snapshot!(rendered(&chat.workflow_monitor, /*width*/ 80));

    chat.handle_key_event(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    let event = event_rx.try_recv().expect("workflow drill event");
    assert!(matches!(
        event,
        crate::app_event::AppEvent::SelectWorkflowAgentThread {
            thread_id,
            run_id,
            node_id: 1,
        } if thread_id == completed_thread_id && run_id == "run-navigation"
    ));

    chat.handle_key_event(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Esc,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert_eq!(chat.workflow_monitor.selection(), None);
}

#[tokio::test]
async fn workflow_stop_requires_explicit_full_run_focus_and_confirmation() {
    let child_thread_id = codex_protocol::ThreadId::new();
    let parent_thread_id = workflow_thread_id();
    let mut monitor = active_monitor("run-stop-confirm", "release-check", "verify");
    apply(
        &mut monitor,
        agent_started(
            "run-stop-confirm",
            /*node_id*/ 7,
            /*parent_node_id*/ None,
            "verifier",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut monitor,
        agent_bound(
            "run-stop-confirm",
            /*node_id*/ 7,
            &child_thread_id.to_string(),
        ),
    );
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(parent_thread_id);
    chat.workflow_monitor = monitor;

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        chat.workflow_monitor.selection(),
        Some(&WorkflowMonitorSelection {
            run_id: "run-stop-confirm".to_string(),
            node_id: 7,
        })
    );
    assert!(
        chat.handle_workflow_monitor_key_event(KeyEvent::new(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
        ))
    );
    assert!(event_rx.try_recv().is_err());

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(
        chat.workflow_monitor.selected_run_id(),
        Some("run-stop-confirm")
    );
    insta::assert_snapshot!(
        "workflow_stop_selected_running_footer",
        rendered(&chat.workflow_monitor, /*width*/ 90)
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    assert!(event_rx.try_recv().is_err());
    insta::assert_snapshot!(
        "workflow_stop_confirmation",
        crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 80)
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        event_rx.try_recv().is_err(),
        "default confirmation selection must cancel"
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let event = event_rx.try_recv().expect("confirmed workflow stop event");
    assert!(matches!(
        event,
        crate::app_event::AppEvent::RequestWorkflowStop { thread_id, run_id }
            if thread_id == parent_thread_id && run_id == "run-stop-confirm"
    ));
}

#[tokio::test]
async fn workflow_stop_states_are_targeted_bounded_and_retryable() {
    let thread_id = workflow_thread_id();
    let (mut chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = selected_run_monitor("run-stop-state", "release-check");

    assert!(
        chat.on_workflow_stop_requested(thread_id, "run-stop-state"),
        "first request should enter pending"
    );
    assert_eq!(
        chat.workflow_monitor.runs[0].stop_request,
        WorkflowStopRequestState::Pending { thread_id }
    );
    for key_event in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Repeat),
    ] {
        assert!(chat.handle_workflow_monitor_key_event(key_event));
    }
    assert_eq!(
        (
            chat.workflow_monitor.selected_run_id(),
            &chat.workflow_monitor.runs[0].stop_request,
        ),
        (
            Some("run-stop-state"),
            &WorkflowStopRequestState::Pending { thread_id }
        )
    );
    let pending = rendered(&chat.workflow_monitor, /*width*/ 90);
    assert!(!chat.on_workflow_stop_requested(thread_id, "run-stop-state"));
    assert_eq!(
        chat.workflow_monitor.runs[0].stop_request,
        WorkflowStopRequestState::Pending { thread_id }
    );

    chat.on_workflow_stop_finished(
        thread_id,
        "run-stop-state",
        Ok(WorkflowStopDisposition::Applied),
    );
    assert_eq!(
        chat.workflow_monitor.runs[0].stop_request,
        WorkflowStopRequestState::Applied
    );
    let applied = rendered(&chat.workflow_monitor, /*width*/ 90);

    chat.workflow_monitor = selected_run_monitor("run-stop-joined", "joined-stop");
    assert!(chat.on_workflow_stop_requested(thread_id, "run-stop-joined"));
    chat.on_workflow_stop_finished(
        thread_id,
        "run-stop-joined",
        Ok(WorkflowStopDisposition::AlreadyRequested),
    );
    assert_eq!(
        chat.workflow_monitor.runs[0].stop_request,
        WorkflowStopRequestState::AlreadyRequested
    );
    let already_requested = rendered(&chat.workflow_monitor, /*width*/ 90);

    chat.workflow_monitor = selected_run_monitor("run-stop-error", "failed-stop");
    assert!(chat.on_workflow_stop_requested(thread_id, "run-stop-error"));
    let long_error = format!("remote cleanup failed {}", "x".repeat(700));
    chat.on_workflow_stop_finished(thread_id, "run-stop-error", Err(long_error));
    let WorkflowStopRequestState::Failed(error) = &chat.workflow_monitor.runs[0].stop_request
    else {
        panic!("expected bounded stop error");
    };
    assert_eq!(error.chars().count(), 512);
    assert!(error.ends_with('…'));
    assert!(chat.workflow_monitor.selected_run_can_stop());
    assert!(chat.on_workflow_stop_requested(thread_id, "run-stop-error"));
    chat.on_workflow_stop_finished(
        thread_id,
        "run-stop-error",
        Err("remote cleanup failed".to_string()),
    );
    let failed = rendered(&chat.workflow_monitor, /*width*/ 90);

    chat.workflow_monitor = selected_run_monitor("run-stop-inactive", "inactive-stop");
    apply(
        &mut chat.workflow_monitor,
        completed(
            "run-stop-inactive",
            CollabAgentStatus::Interrupted,
            /*message*/ None,
            /*spent*/ 12,
            /*total*/ Some(100),
        ),
    );
    assert!(!chat.workflow_monitor.selected_run_can_stop());
    let inactive = rendered(&chat.workflow_monitor, /*width*/ 90);

    insta::assert_snapshot!(
        "workflow_stop_request_states",
        format!(
            "PENDING\n{pending}\n\nAPPLIED\n{applied}\n\nALREADY REQUESTED\n{already_requested}\n\nFAILED\n{failed}\n\nINACTIVE\n{inactive}"
        )
    );
}

#[tokio::test]
async fn workflow_stop_result_settles_retained_parent_run_while_child_is_active() {
    let parent_thread_id = workflow_thread_id();
    let child_thread_id = codex_protocol::ThreadId::new();
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(parent_thread_id);
    chat.workflow_monitor = selected_run_monitor("run-stop-switch", "release-check");

    assert!(chat.on_workflow_stop_requested(parent_thread_id, "run-stop-switch"));
    chat.thread_id = Some(child_thread_id);
    chat.on_workflow_stop_finished(
        child_thread_id,
        "run-stop-switch",
        Ok(WorkflowStopDisposition::Applied),
    );
    assert_eq!(
        chat.workflow_monitor.runs[0].stop_request,
        WorkflowStopRequestState::Pending {
            thread_id: parent_thread_id,
        }
    );

    chat.on_workflow_stop_finished(
        parent_thread_id,
        "run-stop-switch",
        Ok(WorkflowStopDisposition::Applied),
    );
    assert_eq!(
        chat.workflow_monitor.runs[0].stop_request,
        WorkflowStopRequestState::Applied
    );
    assert!(
        event_rx.try_recv().is_err(),
        "an inactive parent result must not write stop history"
    );
}

#[test]
fn workflow_stop_never_targets_compact_or_saturated_runs() {
    let mut monitor = WorkflowMonitor::default();
    for index in 0..(MAX_ACTIVE_RUNS + MAX_SUMMARIZED_RUNS + 2) {
        apply(
            &mut monitor,
            started(
                &format!("run-overflow-{index}"),
                &format!("overflow {index}"),
                &[],
            ),
        );
    }

    let summarized_run_id = monitor
        .summarized_runs
        .front()
        .expect("compact run")
        .run_id
        .clone();
    monitor.selected_run_id = Some(summarized_run_id);
    assert!(!monitor.selected_run_can_stop());

    let saturated_run_id = monitor
        .saturated_run_ids
        .front()
        .expect("saturated run")
        .clone();
    monitor.selected_run_id = Some(saturated_run_id);
    assert!(!monitor.selected_run_can_stop());
}

#[tokio::test]
async fn workflow_save_requires_explicit_completed_full_run_and_exact_name_confirmation() {
    let thread_id = workflow_thread_id();
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = completed_run_monitor("run-save-confirm", "release-check");

    assert!(
        !chat.handle_workflow_monitor_key_event(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        ))
    );
    assert!(event_rx.try_recv().is_err());

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        chat.workflow_monitor.selected_run_id(),
        Some("run-save-confirm")
    );
    insta::assert_snapshot!(
        "workflow_save_selected_completed_footer",
        rendered(&chat.workflow_monitor, /*width*/ 90)
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    insta::assert_snapshot!(
        "workflow_save_exact_name_confirmation",
        crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 90)
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let target = match event_rx.try_recv().expect("open save scope event") {
        crate::app_event::AppEvent::OpenWorkflowSaveScope { target } => target,
        event => panic!("unexpected save-name event: {event:?}"),
    };
    assert_eq!(
        target,
        WorkflowSaveRunTarget {
            thread_id,
            run_id: "run-save-confirm".to_string(),
            name: "release-check".to_string(),
        }
    );

    assert!(chat.open_workflow_save_scope_picker(target.clone()));
    insta::assert_snapshot!(
        "workflow_save_scope_picker_with_project_warning",
        crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 90)
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let personal_request = match event_rx.try_recv().expect("personal save request") {
        crate::app_event::AppEvent::RequestWorkflowSave { request } => request,
        event => panic!("unexpected personal save event: {event:?}"),
    };
    assert_eq!(
        personal_request,
        save_request(
            target.clone(),
            WorkflowSaveScope::Personal,
            WorkflowSaveIntent::Create
        )
    );

    assert!(chat.open_workflow_save_scope_picker(target.clone()));
    chat.handle_key_event(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let project_request = match event_rx.try_recv().expect("project save request") {
        crate::app_event::AppEvent::RequestWorkflowSave { request } => request,
        event => panic!("unexpected project save event: {event:?}"),
    };
    assert_eq!(
        project_request,
        save_request(
            target,
            WorkflowSaveScope::Project,
            WorkflowSaveIntent::Create
        )
    );
}

#[tokio::test]
async fn workflow_save_preserves_names_longer_than_the_monitor_display_bound() {
    let thread_id = workflow_thread_id();
    let exact_name = "a".repeat(/*n*/ 140);
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = completed_run_monitor("run-save-long-name", &exact_name);
    chat.workflow_monitor.selected_run_id = Some("run-save-long-name".to_string());

    assert_ne!(chat.workflow_monitor.runs[0].model.name, exact_name);
    assert_eq!(
        chat.workflow_monitor.runs[0]
            .identity
            .durable_name
            .as_deref(),
        Some(exact_name.as_str())
    );
    assert!(chat.open_workflow_save_dialog());
    let popup = crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 240);
    assert!(
        popup.contains(&exact_name),
        "exact name missing from popup: {popup}"
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let event = event_rx.try_recv().expect("exact-name scope event");
    assert!(matches!(
        event,
        crate::app_event::AppEvent::OpenWorkflowSaveScope { target }
            if target.name == exact_name
    ));
}

#[tokio::test]
async fn workflow_save_conflict_requires_second_cancel_default_confirmation() {
    let thread_id = workflow_thread_id();
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = completed_run_monitor("run-save-conflict", "release-check");
    chat.workflow_monitor.selected_run_id = Some("run-save-conflict".to_string());
    let request = save_request(
        WorkflowSaveRunTarget {
            thread_id,
            run_id: "run-save-conflict".to_string(),
            name: "release-check".to_string(),
        },
        WorkflowSaveScope::Project,
        WorkflowSaveIntent::Create,
    );

    assert!(chat.on_workflow_save_requested(&request));
    chat.on_workflow_save_finished(request.clone(), Ok(WorkflowSaveDisposition::Conflict));
    insta::assert_snapshot!(
        "workflow_save_overwrite_confirmation",
        crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 90)
    );

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let cancel_event = loop {
        let event = event_rx.try_recv().expect("default conflict cancel");
        if matches!(event, crate::app_event::AppEvent::InsertHistoryCell(_)) {
            continue;
        }
        break event;
    };
    let cancel_target = match cancel_event {
        crate::app_event::AppEvent::CancelWorkflowSaveConflict { target } => target,
        event => panic!("unexpected default conflict event: {event:?}"),
    };
    assert_eq!(cancel_target, request.target);
    chat.cancel_workflow_save_conflict(cancel_target);
    assert_eq!(
        chat.workflow_monitor.runs[0].save_request,
        WorkflowSaveRequestState::Idle
    );

    assert!(chat.on_workflow_save_requested(&request));
    chat.on_workflow_save_finished(request.clone(), Ok(WorkflowSaveDisposition::Conflict));
    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let overwrite_event = loop {
        let event = event_rx.try_recv().expect("explicit overwrite request");
        if matches!(event, crate::app_event::AppEvent::InsertHistoryCell(_)) {
            continue;
        }
        break event;
    };
    let overwrite = match overwrite_event {
        crate::app_event::AppEvent::RequestWorkflowSave { request } => request,
        event => panic!("unexpected overwrite event: {event:?}"),
    };
    assert_eq!(
        overwrite,
        WorkflowSaveRequest {
            target: request.target,
            intent: WorkflowSaveIntent::Overwrite,
        }
    );
}

#[tokio::test]
async fn workflow_save_states_are_targeted_bounded_and_ignore_stale_results() {
    let thread_id = workflow_thread_id();
    let (mut chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = completed_run_monitor("run-save-state", "release-check");
    chat.workflow_monitor.selected_run_id = Some("run-save-state".to_string());
    let run_target = WorkflowSaveRunTarget {
        thread_id,
        run_id: "run-save-state".to_string(),
        name: "release-check".to_string(),
    };
    let create = save_request(
        run_target.clone(),
        WorkflowSaveScope::Personal,
        WorkflowSaveIntent::Create,
    );

    assert!(chat.on_workflow_save_requested(&create));
    assert!(!chat.on_workflow_save_requested(&create));
    let pending = rendered(&chat.workflow_monitor, /*width*/ 90);
    let wrong_run = save_request(
        WorkflowSaveRunTarget {
            thread_id,
            run_id: "run-save-other".to_string(),
            name: "release-check".to_string(),
        },
        WorkflowSaveScope::Personal,
        WorkflowSaveIntent::Create,
    );
    chat.on_workflow_save_finished(wrong_run, Ok(WorkflowSaveDisposition::Created));
    assert_eq!(
        chat.workflow_monitor.runs[0].save_request,
        WorkflowSaveRequestState::Pending(create.clone())
    );
    let stale = save_request(
        run_target,
        WorkflowSaveScope::Project,
        WorkflowSaveIntent::Create,
    );
    chat.on_workflow_save_finished(stale, Ok(WorkflowSaveDisposition::Created));
    assert_eq!(
        chat.workflow_monitor.runs[0].save_request,
        WorkflowSaveRequestState::Pending(create.clone())
    );
    chat.on_workflow_save_finished(create.clone(), Ok(WorkflowSaveDisposition::Created));
    let created = rendered(&chat.workflow_monitor, /*width*/ 90);

    assert!(chat.on_workflow_save_requested(&create));
    chat.on_workflow_save_finished(create.clone(), Ok(WorkflowSaveDisposition::Conflict));
    let conflict = rendered(&chat.workflow_monitor, /*width*/ 90);
    chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let overwrite = WorkflowSaveRequest {
        target: create.target.clone(),
        intent: WorkflowSaveIntent::Overwrite,
    };
    assert!(chat.on_workflow_save_requested(&overwrite));
    chat.on_workflow_save_finished(overwrite, Ok(WorkflowSaveDisposition::Overwritten));
    let overwritten = rendered(&chat.workflow_monitor, /*width*/ 90);

    assert!(chat.on_workflow_save_requested(&create));
    let long_error = format!("registry unavailable {}", "x".repeat(/*n*/ 700));
    chat.on_workflow_save_finished(create, Err(long_error));
    let WorkflowSaveRequestState::Failed { error, .. } =
        &chat.workflow_monitor.runs[0].save_request
    else {
        panic!("expected bounded save failure");
    };
    assert_eq!(error.chars().count(), MAX_SAVE_ERROR_CHARS);
    assert!(error.ends_with('…'));
    let failed = rendered(&chat.workflow_monitor, /*width*/ 90);

    insta::assert_snapshot!(
        "workflow_save_request_states",
        format!(
            "PENDING\n{pending}\n\nCREATED\n{created}\n\nCONFLICT\n{conflict}\n\nOVERWRITTEN\n{overwritten}\n\nFAILED\n{failed}"
        )
    );
}

#[tokio::test]
async fn workflow_save_conflict_does_not_replace_an_unrelated_modal() {
    let thread_id = workflow_thread_id();
    let (mut chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = completed_run_monitor("run-save-modal", "release-check");
    chat.workflow_monitor.selected_run_id = Some("run-save-modal".to_string());
    let request = save_request(
        WorkflowSaveRunTarget {
            thread_id,
            run_id: "run-save-modal".to_string(),
            name: "release-check".to_string(),
        },
        WorkflowSaveScope::Personal,
        WorkflowSaveIntent::Create,
    );
    assert!(chat.on_workflow_save_requested(&request));
    chat.show_selection_view(crate::bottom_pane::SelectionViewParams {
        title: Some("Unrelated modal".to_string()),
        items: vec![crate::bottom_pane::SelectionItem {
            name: "Dismiss".to_string(),
            dismiss_on_select: true,
            ..Default::default()
        }],
        ..Default::default()
    });

    chat.on_workflow_save_finished(request, Ok(WorkflowSaveDisposition::Conflict));
    let popup = crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 90);
    assert!(popup.contains("Unrelated modal"));
    assert!(!popup.contains("Overwrite saved workflow?"));
    chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
    assert!(
        crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 90)
            .contains("Overwrite saved workflow?")
    );
}

#[tokio::test]
async fn workflow_save_result_settles_retained_parent_run_while_child_is_active() {
    let parent_thread_id = workflow_thread_id();
    let child_thread_id = codex_protocol::ThreadId::new();
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(parent_thread_id);
    chat.workflow_monitor = completed_run_monitor("run-save-switch", "release-check");
    chat.workflow_monitor.selected_run_id = Some("run-save-switch".to_string());
    let request = save_request(
        WorkflowSaveRunTarget {
            thread_id: parent_thread_id,
            run_id: "run-save-switch".to_string(),
            name: "release-check".to_string(),
        },
        WorkflowSaveScope::Personal,
        WorkflowSaveIntent::Create,
    );

    assert!(chat.on_workflow_save_requested(&request));
    chat.thread_id = Some(child_thread_id);
    chat.on_workflow_save_finished(request.clone(), Ok(WorkflowSaveDisposition::Conflict));
    assert_eq!(
        chat.workflow_monitor.runs[0].save_request,
        WorkflowSaveRequestState::Conflict(request.target.clone())
    );
    assert!(chat.no_modal_or_popup_active());
    assert!(
        event_rx.try_recv().is_err(),
        "an inactive parent result must not write save history"
    );
    assert!(!chat.open_workflow_save_dialog());

    chat.thread_id = Some(parent_thread_id);
    assert!(
        chat.handle_workflow_monitor_key_event(KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::NONE,
        ))
    );
    assert!(
        crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 90)
            .contains("Overwrite saved workflow?")
    );
}

#[test]
fn workflow_save_ineligible_focus_and_capacity_cases_snapshot() {
    let running = selected_run_monitor("run-save-running", "running");
    assert!(!running.selected_run_can_save());
    let running_rendered = rendered(&running, /*width*/ 90);

    let mut agent = active_monitor("run-save-agent", "agent-focused", "work");
    apply(
        &mut agent,
        agent_started(
            "run-save-agent",
            /*node_id*/ 1,
            /*parent_node_id*/ None,
            "worker",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut agent,
        agent_bound("run-save-agent", /*node_id*/ 1, "thread-save-agent"),
    );
    apply(
        &mut agent,
        agent_completed(
            "run-save-agent",
            /*node_id*/ 1,
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*total_tokens*/ 10,
            /*tool_call_count*/ 0,
            /*returned_null*/ false,
        ),
    );
    apply(
        &mut agent,
        completed(
            "run-save-agent",
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*spent*/ 10,
            /*total*/ Some(10),
        ),
    );
    agent.selection = Some(WorkflowMonitorSelection {
        run_id: "run-save-agent".to_string(),
        node_id: 1,
    });
    assert!(!agent.selected_run_can_save());
    let agent_rendered = rendered(&agent, /*width*/ 90);

    let mut capacity = WorkflowMonitor::default();
    for index in 0..(MAX_ACTIVE_RUNS + MAX_SUMMARIZED_RUNS + 2) {
        apply(
            &mut capacity,
            started(
                &format!("run-save-overflow-{index}"),
                &format!("overflow-{index}"),
                &[],
            ),
        );
    }
    capacity.selected_run_id = capacity
        .summarized_runs
        .front()
        .map(|run| run.run_id.clone());
    assert!(!capacity.selected_run_can_save());
    let capacity_rendered = rendered(&capacity, /*width*/ 90);

    assert!(!running_rendered.contains("s save workflow"));
    assert!(!agent_rendered.contains("s save workflow"));
    assert!(!capacity_rendered.contains("s save workflow"));
    insta::assert_snapshot!(
        "workflow_save_ineligible_cases",
        format!(
            "RUNNING FULL RUN\n{running_rendered}\n\nAGENT FOCUS\n{agent_rendered}\n\nCOMPACT AND SATURATED\n{capacity_rendered}"
        )
    );
}

#[test]
fn run_shutdown_renders_stopped_while_agent_shutdown_remains_shutdown() {
    let mut full = active_monitor("run-stopped", "stoppable", "work");
    apply(
        &mut full,
        agent_started(
            "run-stopped",
            /*node_id*/ 1,
            /*parent_node_id*/ None,
            "worker",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut full,
        agent_bound("run-stopped", /*node_id*/ 1, "thread-stopped-worker"),
    );
    apply(
        &mut full,
        agent_completed(
            "run-stopped",
            /*node_id*/ 1,
            CollabAgentStatus::Shutdown,
            /*message*/ None,
            /*total_tokens*/ 10,
            /*tool_call_count*/ 0,
            /*returned_null*/ false,
        ),
    );
    apply(
        &mut full,
        completed(
            "run-stopped",
            CollabAgentStatus::Shutdown,
            /*message*/ None,
            /*spent*/ 10,
            /*total*/ Some(10),
        ),
    );
    let full_rendered = rendered(&full, /*width*/ 90);
    assert!(full_rendered.contains("stopped"));
    assert!(full_rendered.contains("shutdown"));

    let mut summarized = WorkflowMonitor::default();
    summarized.summarized_runs.push_back(SummarizedRun {
        run_id: "run-stopped-compact".to_string(),
        name: "compact-stop".to_string(),
        status: SummarizedRunStatus::Completed {
            status: AgentStatus::Shutdown,
            terminal_reason: None,
        },
    });
    let summarized_rendered = rendered(&summarized, /*width*/ 90);
    assert!(summarized_rendered.contains("stopped"));
    assert!(!summarized_rendered.contains("shutdown"));

    insta::assert_snapshot!(
        "workflow_run_stopped_labels",
        format!("FULL\n{full_rendered}\n\nCOMPACT\n{summarized_rendered}")
    );
}

#[test]
fn nested_and_empty_groups_snapshot() {
    let mut monitor = active_monitor("run-nested", "deploy", "execute");
    apply(
        &mut monitor,
        group_started(
            "run-nested",
            /*group_id*/ 1,
            /*parent_node_id*/ None,
            WorkflowGroupKind::Parallel,
            /*item_count*/ 2,
        ),
    );
    apply(
        &mut monitor,
        group_started(
            "run-nested",
            /*group_id*/ 2,
            /*parent_node_id*/ Some(1),
            WorkflowGroupKind::Pipeline,
            /*item_count*/ 0,
        ),
    );
    apply(
        &mut monitor,
        group_completed(
            "run-nested",
            /*group_id*/ 2,
            WorkflowGroupKind::Pipeline,
            /*item_count*/ 0,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-nested",
            /*node_id*/ 3,
            /*parent_node_id*/ Some(1),
            "ship-linux",
            "router/gpt-5.5",
            ReasoningEffort::Medium,
        ),
    );

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 80));
}

#[test]
fn out_of_order_agent_completion_snapshot() {
    let mut monitor = active_monitor("run-inverted", "fanout", "work");
    apply(
        &mut monitor,
        group_started(
            "run-inverted",
            /*group_id*/ 10,
            /*parent_node_id*/ None,
            WorkflowGroupKind::Parallel,
            /*item_count*/ 2,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-inverted",
            /*node_id*/ 11,
            /*parent_node_id*/ Some(10),
            "slow-first",
            "gpt-5.5",
            ReasoningEffort::High,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-inverted", /*node_id*/ 11, "thread-slow"),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-inverted",
            /*node_id*/ 12,
            /*parent_node_id*/ Some(10),
            "fast-second",
            "gpt-5.5-mini",
            ReasoningEffort::Low,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-inverted", /*node_id*/ 12, "thread-fast"),
    );
    apply(
        &mut monitor,
        agent_completed(
            "run-inverted",
            /*node_id*/ 12,
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*total_tokens*/ 870,
            /*tool_call_count*/ 2,
            /*returned_null*/ false,
        ),
    );
    apply(
        &mut monitor,
        agent_updated(
            "run-inverted",
            /*node_id*/ 11,
            /*total_tokens*/ 2_100,
            /*tool_call_count*/ 4,
        ),
    );

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 80));
}

#[test]
fn error_and_null_results_snapshot() {
    let mut monitor = active_monitor("run-results", "collect", "work");
    apply(
        &mut monitor,
        group_started(
            "run-results",
            /*group_id*/ 20,
            /*parent_node_id*/ None,
            WorkflowGroupKind::Parallel,
            /*item_count*/ 2,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-results",
            /*node_id*/ 21,
            /*parent_node_id*/ Some(20),
            "api-check",
            "gpt-5.5",
            ReasoningEffort::Medium,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-results", /*node_id*/ 21, "thread-api-check"),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-results",
            /*node_id*/ 22,
            /*parent_node_id*/ Some(20),
            "optional-docs",
            "gpt-5.5-mini",
            ReasoningEffort::Low,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-results", /*node_id*/ 22, "thread-optional-docs"),
    );
    apply(
        &mut monitor,
        agent_completed(
            "run-results",
            /*node_id*/ 21,
            CollabAgentStatus::Errored,
            /*message*/ Some("fixture endpoint returned 503"),
            /*total_tokens*/ 460,
            /*tool_call_count*/ 1,
            /*returned_null*/ false,
        ),
    );
    apply(
        &mut monitor,
        agent_completed(
            "run-results",
            /*node_id*/ 22,
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*total_tokens*/ 220,
            /*tool_call_count*/ 0,
            /*returned_null*/ true,
        ),
    );
    apply(
        &mut monitor,
        group_completed(
            "run-results",
            /*group_id*/ 20,
            WorkflowGroupKind::Parallel,
            /*item_count*/ 2,
        ),
    );

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 80));
}

#[test]
fn terminal_status_and_budget_snapshot() {
    let mut monitor = active_monitor("run-terminal", "release", "verify");
    apply(
        &mut monitor,
        agent_started(
            "run-terminal",
            /*node_id*/ 30,
            /*parent_node_id*/ None,
            "verifier",
            "gpt-5.5",
            ReasoningEffort::XHigh,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-terminal", /*node_id*/ 30, "thread-verifier"),
    );
    apply(
        &mut monitor,
        agent_completed(
            "run-terminal",
            /*node_id*/ 30,
            CollabAgentStatus::Completed,
            /*message*/ Some("all checks passed"),
            /*total_tokens*/ 3_450,
            /*tool_call_count*/ 6,
            /*returned_null*/ false,
        ),
    );
    apply(
        &mut monitor,
        phase(
            "run-terminal",
            /*phase_index*/ 0,
            "verify",
            WorkflowPhaseStatus::Completed,
        ),
    );
    apply(
        &mut monitor,
        completed(
            "run-terminal",
            CollabAgentStatus::Completed,
            /*message*/ Some("release ready"),
            /*spent*/ 3_600,
            /*total*/ Some(8_000),
        ),
    );

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 80));
}

#[test]
fn event_timestamp_elapsed_and_duration_snapshot() {
    const ORIGIN: i64 = 1_800_000_000;
    let mut monitor = WorkflowMonitor::default();
    apply(
        &mut monitor,
        observed_at(
            started("run-timing", "timed release", &["plan", "execute"]),
            ORIGIN,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            phase(
                "run-timing",
                /*phase_index*/ 0,
                "plan",
                WorkflowPhaseStatus::Active,
            ),
            ORIGIN + 10,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            agent_started(
                "run-timing",
                /*node_id*/ 1,
                /*parent_node_id*/ None,
                "planner",
                "gpt-5.5",
                ReasoningEffort::High,
            ),
            ORIGIN + 20,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            agent_bound("run-timing", /*node_id*/ 1, "thread-planner"),
            ORIGIN + 25,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            agent_completed(
                "run-timing",
                /*node_id*/ 1,
                CollabAgentStatus::Completed,
                /*message*/ None,
                /*total_tokens*/ 500,
                /*tool_call_count*/ 2,
                /*returned_null*/ false,
            ),
            ORIGIN + 70,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            phase(
                "run-timing",
                /*phase_index*/ 0,
                "plan",
                WorkflowPhaseStatus::Completed,
            ),
            ORIGIN + 80,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            phase(
                "run-timing",
                /*phase_index*/ 1,
                "execute",
                WorkflowPhaseStatus::Active,
            ),
            ORIGIN + 90,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            agent_started(
                "run-timing",
                /*node_id*/ 2,
                /*parent_node_id*/ None,
                "executor",
                "gpt-5.5",
                ReasoningEffort::Medium,
            ),
            ORIGIN + 100,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            agent_bound("run-timing", /*node_id*/ 2, "thread-executor"),
            ORIGIN + 105,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            agent_updated(
                "run-timing",
                /*node_id*/ 2,
                /*total_tokens*/ 1_200,
                /*tool_call_count*/ 4,
            ),
            ORIGIN + 215,
        ),
    );
    let running = rendered(&monitor, /*width*/ 90);

    apply(
        &mut monitor,
        observed_at(
            agent_completed(
                "run-timing",
                /*node_id*/ 2,
                CollabAgentStatus::Completed,
                /*message*/ None,
                /*total_tokens*/ 1_500,
                /*tool_call_count*/ 5,
                /*returned_null*/ false,
            ),
            ORIGIN + 240,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            phase(
                "run-timing",
                /*phase_index*/ 1,
                "execute",
                WorkflowPhaseStatus::Completed,
            ),
            ORIGIN + 245,
        ),
    );
    apply(
        &mut monitor,
        observed_at(
            completed(
                "run-timing",
                CollabAgentStatus::Completed,
                /*message*/ Some("released"),
                /*spent*/ 2_100,
                /*total*/ Some(4_000),
            ),
            ORIGIN + 260,
        ),
    );
    let completed = rendered(&monitor, /*width*/ 90);

    insta::assert_snapshot!(format!("RUNNING\n{running}\n\nCOMPLETED\n{completed}"));
}

#[test]
fn unmetered_and_zero_limited_budget_snapshot() {
    let mut monitor = WorkflowMonitor::default();
    apply(
        &mut monitor,
        started("run-unmetered", "unmetered work", &[]),
    );
    apply(
        &mut monitor,
        completed(
            "run-unmetered",
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*spent*/ 718,
            /*total*/ None,
        ),
    );
    apply(&mut monitor, started("run-zero-limit", "zero limit", &[]));
    apply(
        &mut monitor,
        completed(
            "run-zero-limit",
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*spent*/ 0,
            /*total*/ Some(0),
        ),
    );

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 80));
}

#[test]
fn concurrent_uuid_v7_runs_render_distinct_compact_ids_snapshot() {
    let mut monitor = WorkflowMonitor::default();
    apply(
        &mut monitor,
        started(
            "019f78be-1234-7abc-8def-0123456789ab",
            "first concurrent run",
            &[],
        ),
    );
    apply(
        &mut monitor,
        started(
            "019f78be-1234-7abc-8def-fedcba987654",
            "second concurrent run",
            &[],
        ),
    );

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 80));
}

#[test]
fn fifth_concurrent_start_response_and_completion_snapshot() {
    let mut monitor = WorkflowMonitor::default();
    for index in 0..MAX_ACTIVE_RUNS {
        apply(
            &mut monitor,
            started(
                &format!("019f78be-1234-7abc-8def-00000000000{index}"),
                &format!("detailed workflow {index}"),
                &[],
            ),
        );
    }
    let overflow_run_id = "019f78be-1234-7abc-8def-fedcba987654";
    assert!(
        monitor
            .register_start_response(overflow_run_id.to_string(), "overflow workflow".to_string(),)
    );
    let starting = rendered(&monitor, /*width*/ 90);

    apply(
        &mut monitor,
        started(overflow_run_id, "overflow workflow", &[]),
    );
    assert_eq!(monitor.runs.len(), MAX_ACTIVE_RUNS);
    assert_eq!(
        monitor.summarized_runs,
        VecDeque::from([SummarizedRun {
            run_id: overflow_run_id.to_string(),
            name: "overflow workflow".to_string(),
            status: SummarizedRunStatus::Running,
        }])
    );
    let running = rendered(&monitor, /*width*/ 90);

    apply(
        &mut monitor,
        completed(
            overflow_run_id,
            CollabAgentStatus::Completed,
            /*message*/ Some("compact result retained"),
            /*spent*/ 120,
            /*total*/ None,
        ),
    );
    let completed = rendered(&monitor, /*width*/ 90);

    insta::assert_snapshot!(format!(
        "START RESPONSE\n{starting}\n\nRUN BEGIN (>4 ACTIVE)\n{running}\n\nRUN END\n{completed}"
    ));
}

#[test]
fn workflow_monitor_saturation_is_explicit_snapshot() {
    let mut monitor = WorkflowMonitor::default();
    let run_count = MAX_ACTIVE_RUNS + MAX_SUMMARIZED_RUNS + 1;
    for index in 0..run_count {
        let run_id = format!("run-{index:04}");
        apply(
            &mut monitor,
            started(&run_id, &format!("workflow {index}"), &[]),
        );
    }

    assert_eq!(monitor.runs.len(), MAX_ACTIVE_RUNS);
    assert_eq!(monitor.summarized_runs.len(), MAX_SUMMARIZED_RUNS);
    assert_eq!(monitor.saturated_run_count, 1);
    let saturated_run_id = format!("run-{:04}", run_count - 1);
    assert!(
        !monitor
            .register_start_response(saturated_run_id.clone(), "duplicate response".to_string(),)
    );
    let saturated = rendered(&monitor, /*width*/ 90);
    assert!(monitor.handle_notification(completed(
        &saturated_run_id,
        CollabAgentStatus::Completed,
        /*message*/ None,
        /*spent*/ 0,
        /*total*/ None,
    )));
    assert_eq!(monitor.saturated_run_count, 0);
    assert!(monitor.saturated_run_ids.is_empty());
    let retired = rendered(&monitor, /*width*/ 90);

    insta::assert_snapshot!(format!(
        "SATURATED\n{saturated}\n\nTERMINAL RETIRED\n{retired}"
    ));
}

#[test]
fn saturated_start_response_is_promoted_when_full_run_capacity_frees() {
    let mut monitor = WorkflowMonitor::default();
    for index in 0..MAX_ACTIVE_RUNS {
        apply(
            &mut monitor,
            started(
                &format!("detailed-run-{index}"),
                &format!("detailed workflow {index}"),
                &[],
            ),
        );
    }
    for index in 0..MAX_SUMMARIZED_RUNS {
        assert!(monitor.register_start_response(
            format!("summary-run-{index}"),
            format!("summary workflow {index}"),
        ));
    }
    let promoted_run_id = "promoted-run";
    assert!(
        monitor
            .register_start_response(promoted_run_id.to_string(), "promoted workflow".to_string(),)
    );
    assert_eq!(monitor.saturated_run_count, 1);

    apply(
        &mut monitor,
        completed(
            "detailed-run-0",
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*spent*/ 0,
            /*total*/ None,
        ),
    );
    apply(
        &mut monitor,
        started(promoted_run_id, "promoted workflow", &[]),
    );

    assert!(
        monitor
            .runs
            .iter()
            .any(|run| run.model.run_id == promoted_run_id)
    );
    assert_eq!(monitor.saturated_run_count, 0);
    assert!(monitor.saturated_run_ids.is_empty());
    assert!(!rendered(&monitor, /*width*/ 90).contains("monitor capacity"));
}

#[test]
fn terminal_event_retires_an_evicted_saturated_run_id() {
    let mut monitor = WorkflowMonitor::default();
    for index in 0..MAX_ACTIVE_RUNS {
        apply(
            &mut monitor,
            started(
                &format!("detailed-run-{index}"),
                &format!("detailed workflow {index}"),
                &[],
            ),
        );
    }
    for index in 0..MAX_SUMMARIZED_RUNS {
        assert!(monitor.register_start_response(
            format!("summary-run-{index}"),
            format!("summary workflow {index}"),
        ));
    }
    let saturated_count = MAX_SATURATED_RUN_IDS.saturating_add(1);
    for index in 0..saturated_count {
        assert!(monitor.register_start_response(
            format!("saturated-run-{index}"),
            format!("saturated workflow {index}"),
        ));
    }
    assert_eq!(
        monitor.saturated_run_count,
        u64::try_from(saturated_count).expect("bounded saturated count")
    );
    assert!(
        !monitor
            .saturated_run_ids
            .iter()
            .any(|run_id| run_id == "saturated-run-0")
    );

    assert!(monitor.handle_notification(completed(
        "saturated-run-0",
        CollabAgentStatus::Completed,
        /*message*/ None,
        /*spent*/ 0,
        /*total*/ None,
    )));

    assert_eq!(
        monitor.saturated_run_count,
        u64::try_from(MAX_SATURATED_RUN_IDS).expect("bounded saturated count")
    );
    assert_eq!(monitor.saturated_run_ids.len(), MAX_SATURATED_RUN_IDS);
}

#[test]
fn narrow_width_snapshot() {
    let mut monitor = active_monitor("run-narrow", "cross-platform verification", "execute");
    apply(
        &mut monitor,
        group_started(
            "run-narrow",
            /*group_id*/ 40,
            /*parent_node_id*/ None,
            WorkflowGroupKind::Pipeline,
            /*item_count*/ 1,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-narrow",
            /*node_id*/ 41,
            /*parent_node_id*/ Some(40),
            "windows integration tests",
            "custom-router/gpt-5.5",
            ReasoningEffort::Ultra,
        ),
    );
    apply(
        &mut monitor,
        agent_bound("run-narrow", /*node_id*/ 41, "thread-windows"),
    );
    apply(
        &mut monitor,
        agent_updated(
            "run-narrow",
            /*node_id*/ 41,
            /*total_tokens*/ 12_450,
            /*tool_call_count*/ 18,
        ),
    );
    apply(
        &mut monitor,
        log(
            "run-narrow",
            "Waiting for the remote Windows executor to finish its integration test matrix.",
        ),
    );

    insta::assert_snapshot!(rendered(&monitor, /*width*/ 30));
}

#[test]
fn invalid_event_preserves_prior_projection() {
    let mut monitor = active_monitor("run-valid", "safe", "work");
    let before = monitor.runs.clone();

    assert!(!monitor.handle_notification(group_completed(
        "run-valid",
        /*group_id*/ 999,
        WorkflowGroupKind::Parallel,
        /*item_count*/ 0,
    )));
    assert_eq!(monitor.runs, before);
}

#[test]
fn completed_run_retention_is_bounded() {
    let mut monitor = WorkflowMonitor::default();
    for run_id in ["run-one", "run-two", "run-three"] {
        apply(&mut monitor, started(run_id, run_id, &[]));
        apply(
            &mut monitor,
            completed(
                run_id,
                CollabAgentStatus::Completed,
                /*message*/ None,
                /*spent*/ 0,
                /*total*/ Some(100),
            ),
        );
    }

    assert_eq!(
        monitor
            .runs
            .iter()
            .map(|run| run.model.run_id.as_str())
            .collect::<Vec<_>>(),
        vec!["run-two", "run-three"]
    );
}

#[test]
fn completed_history_never_displaces_an_active_run() {
    let mut monitor = WorkflowMonitor::default();
    apply(&mut monitor, started("active-old", "active old", &[]));
    apply(&mut monitor, started("done-one", "done one", &[]));
    apply(
        &mut monitor,
        completed(
            "done-one",
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*spent*/ 1,
            /*total*/ Some(10),
        ),
    );
    apply(&mut monitor, started("active-middle", "active middle", &[]));
    apply(&mut monitor, started("done-two", "done two", &[]));
    apply(
        &mut monitor,
        completed(
            "done-two",
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*spent*/ 2,
            /*total*/ Some(10),
        ),
    );
    apply(&mut monitor, started("active-new", "active new", &[]));

    let rendered = rendered(&monitor, /*width*/ 80);
    let headers = rendered
        .lines()
        .filter(|line| line.starts_with("◆ Workflow"))
        .collect::<Vec<_>>();
    assert_eq!(
        headers,
        vec![
            "◆ Workflow active old  active-o  running · elapsed ≥ 0s",
            "◆ Workflow active middle  active-m  running · elapsed ≥ 0s",
            "◆ Workflow active new  active-n  running · elapsed ≥ 0s",
        ]
    );
    assert!(rendered.contains("2 workflow runs omitted"));
}

#[test]
fn name_label_and_model_honor_their_inline_text_bounds() {
    let long_text = "x".repeat(/*n*/ 150);
    let expected = format!("{}…", "x".repeat(/*n*/ 95));
    let mut monitor = WorkflowMonitor::default();
    apply(&mut monitor, started("run-bounds", &long_text, &["work"]));
    apply(
        &mut monitor,
        phase(
            "run-bounds",
            /*phase_index*/ 0,
            "work",
            WorkflowPhaseStatus::Active,
        ),
    );
    apply(
        &mut monitor,
        agent_started(
            "run-bounds",
            /*node_id*/ 1,
            /*parent_node_id*/ None,
            &long_text,
            &long_text,
            ReasoningEffort::Medium,
        ),
    );

    let run = monitor.runs.front().expect("bounded run");
    let codex_core_workflows::WorkflowTopologyNode::Agent(agent) = run
        .model
        .topology
        .get(/*key*/ &1)
        .expect("bounded workflow agent")
    else {
        panic!("expected agent topology node");
    };
    assert_eq!(
        (&run.model.name, &agent.label, &agent.model),
        (&expected, &expected, &expected)
    );
}

fn active_monitor(run_id: &str, name: &str, phase_title: &str) -> WorkflowMonitor {
    let mut monitor = WorkflowMonitor::default();
    apply(&mut monitor, started(run_id, name, &[phase_title]));
    apply(
        &mut monitor,
        phase(
            run_id,
            /*phase_index*/ 0,
            phase_title,
            WorkflowPhaseStatus::Active,
        ),
    );
    monitor
}

fn selected_run_monitor(run_id: &str, name: &str) -> WorkflowMonitor {
    let mut monitor = active_monitor(run_id, name, "work");
    monitor.selected_run_id = Some(run_id.to_string());
    monitor
}

fn completed_run_monitor(run_id: &str, name: &str) -> WorkflowMonitor {
    let mut monitor = active_monitor(run_id, name, "work");
    apply(
        &mut monitor,
        completed(
            run_id,
            CollabAgentStatus::Completed,
            /*message*/ None,
            /*spent*/ 10,
            /*total*/ Some(10),
        ),
    );
    monitor
}

fn save_request(
    run: WorkflowSaveRunTarget,
    scope: WorkflowSaveScope,
    intent: WorkflowSaveIntent,
) -> WorkflowSaveRequest {
    WorkflowSaveRequest {
        target: WorkflowSaveTarget { run, scope },
        intent,
    }
}

fn workflow_thread_id() -> codex_protocol::ThreadId {
    codex_protocol::ThreadId::from_string(THREAD_ID).expect("valid workflow monitor thread id")
}

fn apply_reconciled_status(monitor: &mut WorkflowMonitor, status: WorkflowRunStatus) {
    let revision = monitor
        .begin_status_read(STATUS_READ_RUN_ID)
        .expect("status read");
    assert!(monitor.finish_status_read(STATUS_READ_RUN_ID, revision, Some(status)));
}

fn assert_reconciled_controls(monitor: &mut WorkflowMonitor, can_resume: bool, can_save: bool) {
    monitor.selection = None;
    monitor.selected_run_id = Some(STATUS_READ_RUN_ID.to_string());
    assert!(!monitor.selected_run_can_stop());
    assert!(!monitor.selected_run_can_pause());
    assert_eq!(monitor.selected_run_can_resume(), can_resume);
    assert_eq!(monitor.selected_run_can_save(), can_save);
    monitor.selected_run_id = None;
    monitor.selection = Some(WorkflowMonitorSelection {
        run_id: STATUS_READ_RUN_ID.to_string(),
        node_id: 7,
    });
    assert!(!monitor.selected_agent_can_control());
    monitor.selection = None;
    monitor.selected_run_id = Some(STATUS_READ_RUN_ID.to_string());
}

fn apply(monitor: &mut WorkflowMonitor, notification: WorkflowNotification) {
    assert!(monitor.handle_notification(notification));
}

fn observed_at(mut notification: WorkflowNotification, observed_at: i64) -> WorkflowNotification {
    match &mut notification {
        WorkflowNotification::Started(notification) => notification.started_at = observed_at,
        WorkflowNotification::PhaseChanged(notification) => notification.changed_at = observed_at,
        WorkflowNotification::GroupStarted(notification) => notification.started_at = observed_at,
        WorkflowNotification::GroupCompleted(notification) => {
            notification.completed_at = observed_at;
        }
        WorkflowNotification::AgentStarted(notification) => notification.started_at = observed_at,
        WorkflowNotification::AgentBound(notification) => notification.bound_at = observed_at,
        WorkflowNotification::AgentUpdated(notification) => notification.updated_at = observed_at,
        WorkflowNotification::AgentCompleted(notification) => {
            notification.completed_at = observed_at;
        }
        WorkflowNotification::Log(notification) => notification.emitted_at = observed_at,
        WorkflowNotification::Completed(notification) => notification.completed_at = observed_at,
    }
    notification
}

fn started(run_id: &str, name: &str, phases: &[&str]) -> WorkflowNotification {
    WorkflowNotification::Started(WorkflowStartedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        resumed_from_run_id: None,
        name: name.to_string(),
        phases: phases.iter().map(ToString::to_string).collect(),
        args_digest: "sha256:fixture".to_string(),
        started_at: 1,
    })
}

fn phase(
    run_id: &str,
    phase_index: u64,
    title: &str,
    status: WorkflowPhaseStatus,
) -> WorkflowNotification {
    let changed_at = match status {
        WorkflowPhaseStatus::Active => 2,
        WorkflowPhaseStatus::Completed => 8,
    };
    WorkflowNotification::PhaseChanged(WorkflowPhaseChangedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        phase_index,
        title: title.to_string(),
        status,
        changed_at,
    })
}

fn group_started(
    run_id: &str,
    group_id: u64,
    parent_node_id: Option<u64>,
    kind: WorkflowGroupKind,
    item_count: u64,
) -> WorkflowNotification {
    WorkflowNotification::GroupStarted(WorkflowGroupStartedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        group_id,
        parent_node_id,
        kind,
        item_count,
        started_at: 3,
    })
}

fn group_completed(
    run_id: &str,
    group_id: u64,
    kind: WorkflowGroupKind,
    item_count: u64,
) -> WorkflowNotification {
    WorkflowNotification::GroupCompleted(WorkflowGroupCompletedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        group_id,
        kind,
        item_count,
        completed_at: 4,
    })
}

fn agent_started(
    run_id: &str,
    node_id: u64,
    parent_node_id: Option<u64>,
    label: &str,
    model: &str,
    effort: ReasoningEffort,
) -> WorkflowNotification {
    WorkflowNotification::AgentStarted(WorkflowAgentStartedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        parent_node_id,
        label: label.to_string(),
        phase: None,
        model: model.to_string(),
        effort,
        started_at: 5,
    })
}

fn agent_updated(
    run_id: &str,
    node_id: u64,
    total_tokens: i64,
    tool_call_count: u64,
) -> WorkflowNotification {
    WorkflowNotification::AgentUpdated(WorkflowAgentUpdatedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        token_usage: usage(total_tokens),
        tool_call_count,
        duration_ms: 0,
        updated_at: 6,
    })
}

fn agent_bound(run_id: &str, node_id: u64, child_thread_id: &str) -> WorkflowNotification {
    WorkflowNotification::AgentBound(WorkflowAgentBoundNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        node_id,
        attempt: 0,
        child_thread_id: child_thread_id.to_string(),
        bound_at: 6,
    })
}

#[allow(clippy::too_many_arguments)]
fn agent_completed(
    run_id: &str,
    node_id: u64,
    status: CollabAgentStatus,
    message: Option<&str>,
    total_tokens: i64,
    tool_call_count: u64,
    returned_null: bool,
) -> WorkflowNotification {
    WorkflowNotification::AgentCompleted(WorkflowAgentCompletedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        node_id,
        attempt: 0,
        last_attempt_reason: None,
        status,
        message: message.map(ToString::to_string),
        token_usage: usage(total_tokens),
        tool_call_count,
        duration_ms: 0,
        returned_null,
        completed_at: 7,
    })
}

fn log(run_id: &str, message: &str) -> WorkflowNotification {
    WorkflowNotification::Log(WorkflowLogNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        message: message.to_string(),
        emitted_at: 8,
    })
}

fn completed(
    run_id: &str,
    status: CollabAgentStatus,
    message: Option<&str>,
    spent: i64,
    total: Option<i64>,
) -> WorkflowNotification {
    WorkflowNotification::Completed(WorkflowCompletedNotification {
        thread_id: THREAD_ID.to_string(),
        run_id: run_id.to_string(),
        status,
        message: message.map(ToString::to_string),
        terminal_reason: None,
        spent,
        total,
        completed_at: 9,
    })
}

fn usage(total_tokens: i64) -> TokenUsageBreakdown {
    TokenUsageBreakdown {
        total_tokens,
        input_tokens: total_tokens,
        cached_input_tokens: 0,
        output_tokens: 0,
        reasoning_output_tokens: 0,
    }
}

fn rendered(monitor: &WorkflowMonitor, width: u16) -> String {
    monitor
        .display_lines(width)
        .into_iter()
        .map(|line| {
            line.spans
                .into_iter()
                .map(|span| span.content.into_owned())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

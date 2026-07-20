use super::*;
use crate::app_event::AppEvent;
use crate::app_event::WorkflowAgentControlRequest;
use crate::app_event::WorkflowAgentControlTarget;
use crate::app_event::WorkflowRunControlTarget;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_app_server_protocol::WorkflowAgentAttemptReason;
use codex_app_server_protocol::WorkflowAgentBoundNotification;
use codex_app_server_protocol::WorkflowAgentControlAction;
use codex_app_server_protocol::WorkflowAgentControlResponse;
use codex_app_server_protocol::WorkflowAgentStartedNotification;
use codex_app_server_protocol::WorkflowAgentUpdatedNotification;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowPauseDisposition;
use codex_app_server_protocol::WorkflowRunTerminalReason;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use pretty_assertions::assert_eq;

const RUN_ID: &str = "019f78be-1234-7abc-8def-0123456789ab";
const SOURCE_RUN_ID: &str = "019f78be-1234-7abc-8def-0123456789ac";
const SUCCESSOR_RUN_ID: &str = "019f78be-1234-7abc-8def-0123456789ad";

#[tokio::test]
async fn pause_confirmation_is_cancel_default_and_captures_exact_run() {
    let thread_id = ThreadId::new();
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = selected_run_monitor(thread_id, RUN_ID);

    let footer = rendered(&chat.workflow_monitor, /*width*/ 100);
    assert!(footer.contains("x stop workflow · p pause"));

    assert!(chat.handle_workflow_monitor_key_event(key('p')));
    let popup = crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 90);
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(event_rx.try_recv().is_err(), "pause must default to cancel");

    assert!(chat.handle_workflow_monitor_key_event(key('p')));
    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        event_rx.try_recv().expect("confirmed pause request"),
        AppEvent::RequestWorkflowPause { target }
            if target == WorkflowRunControlTarget {
                thread_id,
                run_id: RUN_ID.to_string(),
            }
    ));

    insta::assert_snapshot!(
        "workflow_pause_footer_and_confirmation",
        format!("FOOTER\n{footer}\n\nCONFIRMATION\n{popup}")
    );
}

#[tokio::test]
async fn pause_states_are_bounded_and_mutually_suppress_stop() {
    let thread_id = ThreadId::new();
    let target = run_target(thread_id, RUN_ID);
    let (mut chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = selected_run_monitor(thread_id, RUN_ID);

    assert!(chat.on_workflow_pause_requested(&target));
    assert!(!chat.on_workflow_stop_requested(thread_id, RUN_ID));
    assert!(!chat.workflow_monitor.selected_run_can_pause());
    assert!(!chat.workflow_monitor.selected_run_can_stop());
    let pending = rendered(&chat.workflow_monitor, /*width*/ 90);

    chat.on_workflow_pause_finished(target.clone(), Ok(WorkflowPauseDisposition::Applied));
    let applied = rendered(&chat.workflow_monitor, /*width*/ 90);

    chat.workflow_monitor = selected_run_monitor(thread_id, RUN_ID);
    assert!(chat.on_workflow_pause_requested(&target));
    chat.on_workflow_pause_finished(
        target.clone(),
        Ok(WorkflowPauseDisposition::AlreadyRequested),
    );
    let already_paused = rendered(&chat.workflow_monitor, /*width*/ 90);

    chat.workflow_monitor = selected_run_monitor(thread_id, RUN_ID);
    assert!(chat.on_workflow_pause_requested(&target));
    chat.on_workflow_pause_finished(target, Err(format!("unavailable {}", "x".repeat(700))));
    let WorkflowPauseRequestState::Failed(error) = &chat.workflow_monitor.runs[0].pause_request
    else {
        panic!("expected bounded pause failure");
    };
    assert_eq!(error.chars().count(), 512);
    assert!(error.ends_with('…'));
    assert!(chat.workflow_monitor.selected_run_can_pause());
    assert!(chat.workflow_monitor.selected_run_can_stop());
    let failed = rendered(&chat.workflow_monitor, /*width*/ 90);

    chat.workflow_monitor = selected_run_monitor(thread_id, RUN_ID);
    assert!(chat.on_workflow_stop_requested(thread_id, RUN_ID));
    assert!(!chat.on_workflow_pause_requested(&run_target(thread_id, RUN_ID)));
    assert!(!chat.workflow_monitor.selected_run_can_pause());

    insta::assert_snapshot!(
        "workflow_pause_request_states",
        format!(
            "PENDING\n{pending}\n\nAPPLIED\n{applied}\n\nALREADY PAUSED\n{already_paused}\n\nFAILED\n{failed}"
        )
    );
}

#[tokio::test]
async fn paused_run_resume_is_exact_idempotent_and_race_safe() {
    let thread_id = ThreadId::new();
    let target = run_target(thread_id, SOURCE_RUN_ID);
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = paused_run_monitor(thread_id, SOURCE_RUN_ID);

    assert!(chat.workflow_monitor.selected_run_can_resume());
    assert!(!chat.workflow_monitor.selected_run_can_save());
    let paused = rendered(&chat.workflow_monitor, /*width*/ 100);
    assert!(paused.contains("paused"));
    assert!(paused.contains("r resume"));

    assert!(chat.handle_workflow_monitor_key_event(key('r')));
    assert!(matches!(
        event_rx.try_recv().expect("resume request"),
        AppEvent::RequestWorkflowResume { target: event_target } if event_target == target
    ));
    assert!(chat.on_workflow_resume_requested(&target));
    assert!(!chat.on_workflow_resume_requested(&target));
    let pending = rendered(&chat.workflow_monitor, /*width*/ 100);

    chat.on_workflow_resume_finished(target.clone(), Ok(SUCCESSOR_RUN_ID.to_string()));
    let response_first = rendered(&chat.workflow_monitor, /*width*/ 100);
    assert_eq!(chat.workflow_monitor.summarized_runs.len(), 1);
    apply(
        &mut chat.workflow_monitor,
        started(
            thread_id,
            SUCCESSOR_RUN_ID,
            Some(SOURCE_RUN_ID),
            "release-check",
        ),
    );
    assert!(chat.workflow_monitor.summarized_runs.is_empty());
    let successor = chat
        .workflow_monitor
        .runs
        .iter()
        .find(|run| run.model.run_id == SUCCESSOR_RUN_ID)
        .expect("full successor after raced run begin");
    assert_eq!(
        successor.model.resumed_from_run_id.as_deref(),
        Some(SOURCE_RUN_ID)
    );
    let resumed = rendered(&chat.workflow_monitor, /*width*/ 100);

    let (mut notification_chat, _app_event_tx, _event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    notification_chat.thread_id = Some(thread_id);
    notification_chat.workflow_monitor = paused_run_monitor(thread_id, SOURCE_RUN_ID);
    apply(
        &mut notification_chat.workflow_monitor,
        started(
            thread_id,
            SUCCESSOR_RUN_ID,
            Some(SOURCE_RUN_ID),
            "release-check",
        ),
    );
    assert!(notification_chat.on_workflow_resume_requested(&target));
    notification_chat.on_workflow_resume_finished(target, Ok(SUCCESSOR_RUN_ID.to_string()));
    assert!(
        notification_chat
            .workflow_monitor
            .summarized_runs
            .is_empty()
    );

    insta::assert_snapshot!(
        "workflow_resume_request_and_lineage",
        format!(
            "PAUSED\n{paused}\n\nPENDING\n{pending}\n\nRESPONSE FIRST\n{response_first}\n\nRUN BEGIN\n{resumed}"
        )
    );
}

#[tokio::test]
async fn agent_skip_and_retry_confirmations_capture_exact_attempt() {
    let thread_id = ThreadId::new();
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = selected_agent_monitor(thread_id, RUN_ID);

    let footer = rendered(&chat.workflow_monitor, /*width*/ 110);
    assert!(footer.contains("x skip · r retry"));

    assert!(chat.handle_workflow_monitor_key_event(key('x')));
    let skip_popup = crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 100);
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(event_rx.try_recv().is_err(), "skip must default to cancel");

    assert!(chat.handle_workflow_monitor_key_event(key('x')));
    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        event_rx.try_recv().expect("exact skip request"),
        AppEvent::RequestWorkflowAgentControl { request }
            if request
                == agent_request(thread_id, RUN_ID, 0, WorkflowAgentControlAction::Skip)
    ));

    assert!(chat.handle_workflow_monitor_key_event(key('r')));
    let retry_popup = crate::chatwidget::tests::render_bottom_popup(&chat, /*width*/ 100);
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(event_rx.try_recv().is_err(), "retry must default to cancel");
    assert!(chat.handle_workflow_monitor_key_event(key('r')));
    chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(
        event_rx.try_recv().expect("exact retry request"),
        AppEvent::RequestWorkflowAgentControl { request }
            if request
                == agent_request(thread_id, RUN_ID, 0, WorkflowAgentControlAction::Retry)
    ));

    insta::assert_snapshot!(
        "workflow_agent_control_footer_and_confirmations",
        format!("FOOTER\n{footer}\n\nSKIP\n{skip_popup}\n\nRETRY\n{retry_popup}")
    );
}

#[tokio::test]
async fn retry_response_before_notifications_blocks_stale_attempt_then_restores_control() {
    let thread_id = ThreadId::new();
    let request = agent_request(thread_id, RUN_ID, 0, WorkflowAgentControlAction::Retry);
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);
    chat.workflow_monitor = selected_agent_monitor(thread_id, RUN_ID);

    assert!(chat.on_workflow_agent_control_requested(&request));
    let pending = rendered(&chat.workflow_monitor, /*width*/ 110);
    chat.on_workflow_agent_control_finished(
        request.clone(),
        Ok(WorkflowAgentControlResponse::RetryScheduled { attempt: 1 }),
    );
    drain_history_events(&mut event_rx);
    assert!(
        !chat.workflow_monitor.selected_agent_can_control(),
        "response must not re-enable the stale generation"
    );
    assert!(!chat.on_workflow_agent_control_requested(&request));
    drain_history_events(&mut event_rx);
    let response_before_notification = rendered(&chat.workflow_monitor, /*width*/ 110);

    apply(
        &mut chat.workflow_monitor,
        agent_started(
            thread_id,
            RUN_ID,
            /*attempt*/ 1,
            Some(WorkflowAgentAttemptReason::UserRetry),
        ),
    );
    assert_eq!(
        chat.workflow_monitor.selection(),
        Some(&WorkflowMonitorSelection {
            run_id: RUN_ID.to_string(),
            node_id: 7,
        })
    );
    assert!(!chat.workflow_monitor.selected_agent_can_control());
    let selected_agent = chat.workflow_monitor.runs[0]
        .model
        .topology
        .get(&7)
        .and_then(|node| match node {
            WorkflowTopologyNode::Agent(agent) => Some(agent),
            WorkflowTopologyNode::Group(_) => None,
        })
        .expect("selected agent");
    assert!(selected_agent.child_thread_id.is_none());
    assert!(
        chat.handle_workflow_monitor_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE,))
    );
    assert!(event_rx.try_recv().is_err());
    let unbound = rendered(&chat.workflow_monitor, /*width*/ 110);

    apply(
        &mut chat.workflow_monitor,
        agent_bound(thread_id, RUN_ID, /*attempt*/ 1),
    );
    assert!(chat.workflow_monitor.selected_agent_can_control());
    let rebound_agent = chat.workflow_monitor.runs[0]
        .model
        .topology
        .get(&7)
        .and_then(|node| match node {
            WorkflowTopologyNode::Agent(agent) => Some(agent),
            WorkflowTopologyNode::Group(_) => None,
        })
        .expect("rebound agent");
    assert!(rebound_agent.child_thread_id.is_some());
    apply(
        &mut chat.workflow_monitor,
        agent_updated(
            thread_id,
            RUN_ID,
            /*attempt*/ 1,
            Some(WorkflowAgentAttemptReason::UserRetry),
            /*duration_ms*/ 1_234,
        ),
    );
    let rebound = rendered(&chat.workflow_monitor, /*width*/ 110);

    insta::assert_snapshot!(
        "workflow_agent_retry_race_and_attempt_rendering",
        format!(
            "PENDING\n{pending}\n\nRESPONSE BEFORE NOTIFICATION\n{response_before_notification}\n\nUNBOUND GENERATION\n{unbound}\n\nREBOUND GENERATION\n{rebound}"
        )
    );
}

#[tokio::test]
async fn agent_control_results_are_correlated_bounded_and_retryable() {
    let thread_id = ThreadId::new();
    let (mut chat, _app_event_tx, mut event_rx, _op_rx) =
        crate::chatwidget::tests::make_chatwidget_manual_with_sender().await;
    chat.thread_id = Some(thread_id);

    chat.workflow_monitor = selected_agent_monitor(thread_id, RUN_ID);
    let skip = agent_request(thread_id, RUN_ID, 0, WorkflowAgentControlAction::Skip);
    assert!(chat.on_workflow_agent_control_requested(&skip));
    chat.on_workflow_agent_control_finished(skip, Ok(WorkflowAgentControlResponse::Skipped));
    drain_history_events(&mut event_rx);
    assert!(!chat.workflow_monitor.selected_agent_can_control());
    let skipped = rendered(&chat.workflow_monitor, /*width*/ 100);

    chat.workflow_monitor = selected_agent_monitor(thread_id, RUN_ID);
    let retry = agent_request(thread_id, RUN_ID, 0, WorkflowAgentControlAction::Retry);
    assert!(chat.on_workflow_agent_control_requested(&retry));
    chat.on_workflow_agent_control_finished(
        retry,
        Ok(WorkflowAgentControlResponse::RetryLimitReached),
    );
    drain_history_events(&mut event_rx);
    assert!(!chat.workflow_monitor.selected_agent_can_control());
    let retry_limit = rendered(&chat.workflow_monitor, /*width*/ 100);

    chat.workflow_monitor = selected_agent_monitor(thread_id, RUN_ID);
    let failed = agent_request(thread_id, RUN_ID, 0, WorkflowAgentControlAction::Retry);
    assert!(chat.on_workflow_agent_control_requested(&failed));
    chat.on_workflow_agent_control_finished(
        failed.clone(),
        Err(format!("unavailable {}", "x".repeat(700))),
    );
    drain_history_events(&mut event_rx);
    let WorkflowAgentControlRequestState::Failed { error, .. } = chat.workflow_monitor.runs[0]
        .agent_control_requests
        .get(&7)
        .expect("failed state")
    else {
        panic!("expected failed control state");
    };
    assert_eq!(error.chars().count(), 512);
    assert!(error.ends_with('…'));
    assert!(chat.workflow_monitor.selected_agent_can_control());
    let failed_state = rendered(&chat.workflow_monitor, /*width*/ 100);
    assert!(chat.on_workflow_agent_control_requested(&failed));
    let retried = rendered(&chat.workflow_monitor, /*width*/ 100);

    insta::assert_snapshot!(
        "workflow_agent_control_result_states",
        format!(
            "SKIPPED\n{skipped}\n\nRETRY LIMIT\n{retry_limit}\n\nFAILED\n{failed_state}\n\nRETRIED\n{retried}"
        )
    );
}

#[test]
fn malformed_or_oversized_child_thread_ids_are_bounded_and_not_controllable() {
    let thread_id = ThreadId::new();
    for child_thread_id in ["not-a-thread".to_string(), "x".repeat(4_096)] {
        let mut monitor = selected_run_monitor(thread_id, RUN_ID);
        apply(
            &mut monitor,
            agent_started(thread_id, RUN_ID, /*attempt*/ 0, None),
        );
        apply(
            &mut monitor,
            agent_bound_with_id(thread_id, RUN_ID, /*attempt*/ 0, child_thread_id),
        );
        monitor.selection = Some(WorkflowMonitorSelection {
            run_id: RUN_ID.to_string(),
            node_id: 7,
        });
        monitor.selected_run_id = None;
        let WorkflowTopologyNode::Agent(agent) =
            monitor.runs[0].model.topology.get(&7).expect("agent")
        else {
            panic!("expected agent");
        };
        assert!(
            agent
                .child_thread_id
                .as_ref()
                .is_some_and(|child_thread_id| child_thread_id.chars().count() <= 64)
        );
        assert!(!monitor.selected_agent_can_control());
    }
}

#[test]
fn exact_terminal_reasons_drive_status_and_save_eligibility() {
    let thread_id = ThreadId::new();
    let mut rendered_reasons = Vec::new();
    for (reason, status, label, saveable) in [
        (
            WorkflowRunTerminalReason::Completed,
            CollabAgentStatus::Completed,
            "COMPLETED",
            true,
        ),
        (
            WorkflowRunTerminalReason::Failed,
            CollabAgentStatus::Errored,
            "FAILED",
            false,
        ),
        (
            WorkflowRunTerminalReason::Interrupted,
            CollabAgentStatus::Interrupted,
            "INTERRUPTED",
            false,
        ),
        (
            WorkflowRunTerminalReason::Stopped,
            CollabAgentStatus::Shutdown,
            "STOPPED",
            false,
        ),
        (
            WorkflowRunTerminalReason::Paused,
            CollabAgentStatus::Interrupted,
            "PAUSED",
            false,
        ),
    ] {
        let mut monitor = selected_run_monitor(thread_id, RUN_ID);
        apply(&mut monitor, completed(thread_id, RUN_ID, status, reason));
        assert_eq!(monitor.selected_run_can_save(), saveable);
        rendered_reasons.push(format!("{label}\n{}", rendered(&monitor, /*width*/ 90)));
    }

    insta::assert_snapshot!(
        "workflow_exact_terminal_reasons",
        rendered_reasons.join("\n\n")
    );
}

fn selected_run_monitor(thread_id: ThreadId, run_id: &str) -> WorkflowMonitor {
    let mut monitor = WorkflowMonitor::default();
    apply(
        &mut monitor,
        started(thread_id, run_id, None, "release-check"),
    );
    monitor.selected_run_id = Some(run_id.to_string());
    monitor
}

fn paused_run_monitor(thread_id: ThreadId, run_id: &str) -> WorkflowMonitor {
    let mut monitor = selected_run_monitor(thread_id, run_id);
    apply(
        &mut monitor,
        completed(
            thread_id,
            run_id,
            CollabAgentStatus::Interrupted,
            WorkflowRunTerminalReason::Paused,
        ),
    );
    monitor
}

fn selected_agent_monitor(thread_id: ThreadId, run_id: &str) -> WorkflowMonitor {
    let mut monitor = selected_run_monitor(thread_id, run_id);
    apply(
        &mut monitor,
        agent_started(thread_id, run_id, /*attempt*/ 0, None),
    );
    apply(&mut monitor, agent_bound(thread_id, run_id, /*attempt*/ 0));
    monitor.selection = Some(WorkflowMonitorSelection {
        run_id: run_id.to_string(),
        node_id: 7,
    });
    monitor.selected_run_id = None;
    monitor
}

fn apply(monitor: &mut WorkflowMonitor, notification: WorkflowNotification) {
    assert!(monitor.handle_notification(notification));
}

fn started(
    thread_id: ThreadId,
    run_id: &str,
    resumed_from_run_id: Option<&str>,
    name: &str,
) -> WorkflowNotification {
    WorkflowNotification::Started(WorkflowStartedNotification {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        resumed_from_run_id: resumed_from_run_id.map(ToString::to_string),
        name: name.to_string(),
        phases: Vec::new(),
        args_digest: "sha256:fixture".to_string(),
        started_at: 1,
    })
}

fn agent_started(
    thread_id: ThreadId,
    run_id: &str,
    attempt: u32,
    last_attempt_reason: Option<WorkflowAgentAttemptReason>,
) -> WorkflowNotification {
    WorkflowNotification::AgentStarted(WorkflowAgentStartedNotification {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        node_id: 7,
        attempt,
        last_attempt_reason,
        parent_node_id: None,
        label: "worker".to_string(),
        phase: None,
        model: "gpt-5.5".to_string(),
        effort: ReasoningEffort::High,
        started_at: 2,
    })
}

fn agent_bound(thread_id: ThreadId, run_id: &str, attempt: u32) -> WorkflowNotification {
    agent_bound_with_id(thread_id, run_id, attempt, ThreadId::new().to_string())
}

fn agent_bound_with_id(
    thread_id: ThreadId,
    run_id: &str,
    attempt: u32,
    child_thread_id: String,
) -> WorkflowNotification {
    WorkflowNotification::AgentBound(WorkflowAgentBoundNotification {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        node_id: 7,
        attempt,
        child_thread_id,
        bound_at: 3,
    })
}

fn agent_updated(
    thread_id: ThreadId,
    run_id: &str,
    attempt: u32,
    last_attempt_reason: Option<WorkflowAgentAttemptReason>,
    duration_ms: u64,
) -> WorkflowNotification {
    WorkflowNotification::AgentUpdated(WorkflowAgentUpdatedNotification {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        node_id: 7,
        attempt,
        last_attempt_reason,
        token_usage: usage(/*total_tokens*/ 1_250),
        tool_call_count: 3,
        duration_ms,
        updated_at: 4,
    })
}

fn completed(
    thread_id: ThreadId,
    run_id: &str,
    status: CollabAgentStatus,
    terminal_reason: WorkflowRunTerminalReason,
) -> WorkflowNotification {
    WorkflowNotification::Completed(WorkflowCompletedNotification {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        status,
        message: None,
        terminal_reason: Some(terminal_reason),
        spent: 0,
        total: None,
        completed_at: 5,
    })
}

fn run_target(thread_id: ThreadId, run_id: &str) -> WorkflowRunControlTarget {
    WorkflowRunControlTarget {
        thread_id,
        run_id: run_id.to_string(),
    }
}

fn agent_request(
    thread_id: ThreadId,
    run_id: &str,
    attempt: u32,
    action: WorkflowAgentControlAction,
) -> WorkflowAgentControlRequest {
    WorkflowAgentControlRequest {
        target: WorkflowAgentControlTarget {
            thread_id,
            run_id: run_id.to_string(),
            node_id: 7,
            attempt,
        },
        action,
    }
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

fn key(ch: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE)
}

fn rendered(monitor: &WorkflowMonitor, width: u16) -> String {
    monitor
        .display_lines(width)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn drain_history_events(event_rx: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
    while let Ok(event) = event_rx.try_recv() {
        assert!(matches!(event, AppEvent::InsertHistoryCell(_)));
    }
}

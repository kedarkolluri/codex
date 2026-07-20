use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use app_test_support::TestAppServer;
use app_test_support::create_fake_rollout;
use app_test_support::create_mock_responses_server_repeating_assistant;
use app_test_support::rollout_path;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_core_workflows::WorkflowRunModel;
use codex_features::Feature;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowAgentBeginEvent;
use codex_protocol::protocol::WorkflowAgentBoundEvent;
use codex_protocol::protocol::WorkflowAgentEndEvent;
use codex_protocol::protocol::WorkflowAgentUpdatedEvent;
use codex_protocol::protocol::WorkflowEvent;
use codex_protocol::protocol::WorkflowGroupBeginEvent;
use codex_protocol::protocol::WorkflowGroupEndEvent;
use codex_protocol::protocol::WorkflowGroupKind;
use codex_protocol::protocol::WorkflowLogEvent;
use codex_protocol::protocol::WorkflowPhaseBeginEvent;
use codex_protocol::protocol::WorkflowPhaseEndEvent;
use codex_protocol::protocol::WorkflowRunBeginEvent;
use codex_protocol::protocol::WorkflowRunEndEvent;
use codex_protocol::protocol::WorkflowRunTerminalReason;
use codex_rollout::append_rollout_item_to_path;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

#[cfg(windows)]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(not(windows))]
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(10);

const COMPLETED_RUN_ID: &str = "run-completed";
const ACTIVE_RUN_ID: &str = "run-active";
const RETAINED_LOG_COUNT: usize = 100;

#[tokio::test]
async fn thread_resume_replays_bounded_workflow_progress_after_response() -> Result<()> {
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    let feature_flags = BTreeMap::from([(Feature::Workflow, true)]);
    write_mock_responses_config_toml(
        codex_home.path(),
        &model_server.uri(),
        &feature_flags,
        /*auto_compact_limit*/ 200_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;

    let filename_ts = "2025-01-05T12-00-00";
    let thread_id = create_fake_rollout(
        codex_home.path(),
        filename_ts,
        "2025-01-05T12:00:00Z",
        "Saved workflow run",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let path = rollout_path(codex_home.path(), filename_ts, &thread_id);
    append_workflow_fixture(&path).await?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let initialized = timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.initialize_with_capabilities(
            ClientInfo {
                name: "workflow-resume-integration-test".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        ),
    )
    .await??;
    assert!(matches!(initialized, JSONRPCMessage::Response(_)));

    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;

    // Resume owns the snapshot ordering contract: unrelated notifications may race with the
    // request, but no connection-scoped workflow replay may precede its response.
    let response = timeout(
        DEFAULT_READ_TIMEOUT,
        read_resume_response_before_workflow(&mut app_server, RequestId::Integer(resume_id)),
    )
    .await??;
    assert_eq!(response.id, RequestId::Integer(resume_id));
    let ThreadResumeResponse { thread, .. } = to_response(response)?;
    assert_eq!(thread.id, thread_id);

    let expected = expected_workflow_replay(&thread_id);
    let actual = timeout(
        DEFAULT_READ_TIMEOUT,
        read_workflow_replay(&mut app_server, expected.len()),
    )
    .await??;
    assert_eq!(actual, expected);

    // A following request is an ordering fence: replay is connection-scoped and enqueued before
    // resume completes, so any uncoalesced update or over-cap log would appear before this reply.
    let read_id = app_server
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread_id.clone(),
            include_turns: false,
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    let unexpected_workflow_notifications = app_server
        .pending_notification_methods()
        .into_iter()
        .filter(|method| method.starts_with("workflow/"))
        .collect::<Vec<_>>();
    assert_eq!(unexpected_workflow_notifications, Vec::<String>::new());

    Ok(())
}

#[tokio::test]
async fn thread_resume_does_not_replay_workflow_progress_when_feature_is_disabled() -> Result<()> {
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    write_mock_responses_config_toml(
        codex_home.path(),
        &model_server.uri(),
        &BTreeMap::new(),
        /*auto_compact_limit*/ 200_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;

    let filename_ts = "2025-01-05T12-00-01";
    let thread_id = create_fake_rollout(
        codex_home.path(),
        filename_ts,
        "2025-01-05T12:00:01Z",
        "Saved workflow run while disabled",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let path = rollout_path(codex_home.path(), filename_ts, &thread_id);
    append_workflow_fixture(&path).await?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.initialize_with_capabilities(
            ClientInfo {
                name: "workflow-resume-disabled-test".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        ),
    )
    .await??;

    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        read_resume_response_before_workflow(&mut app_server, RequestId::Integer(resume_id)),
    )
    .await??;

    // Fence all messages enqueued by resume. Historical workflow events remain on disk but must
    // not be projected to a client while the runtime feature is disabled.
    let read_id = app_server
        .send_thread_read_request(ThreadReadParams {
            thread_id: thread_id.clone(),
            include_turns: false,
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    assert_eq!(
        app_server
            .pending_notification_methods()
            .into_iter()
            .filter(|method| method.starts_with("workflow/"))
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );

    // The thread is now live in this app-server, so a second resume exercises the running-thread
    // listener path rather than cold reconstruction. It must apply the same feature gate.
    let hot_resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        read_resume_response_before_workflow(&mut app_server, RequestId::Integer(hot_resume_id)),
    )
    .await??;
    let hot_read_id = app_server
        .send_thread_read_request(ThreadReadParams {
            thread_id,
            include_turns: false,
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(hot_read_id)),
    )
    .await??;
    assert_eq!(
        app_server
            .pending_notification_methods()
            .into_iter()
            .filter(|method| method.starts_with("workflow/"))
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );

    Ok(())
}

#[tokio::test]
async fn thread_resume_prioritizes_an_old_active_run_over_newer_terminal_runs() -> Result<()> {
    let model_server = create_mock_responses_server_repeating_assistant("unused").await;
    let codex_home = TempDir::new()?;
    let feature_flags = BTreeMap::from([(Feature::Workflow, true)]);
    write_mock_responses_config_toml(
        codex_home.path(),
        &model_server.uri(),
        &feature_flags,
        /*auto_compact_limit*/ 200_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;

    let filename_ts = "2025-01-05T12-00-02";
    let thread_id = create_fake_rollout(
        codex_home.path(),
        filename_ts,
        "2025-01-05T12:00:02Z",
        "Active workflow among newer terminal runs",
        Some("mock_provider"),
        /*git_info*/ None,
    )?;
    let path = rollout_path(codex_home.path(), filename_ts, &thread_id);
    let mut events = vec![WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
        run_id: "outer-active".to_string(),
        resumed_from_run_id: None,
        name: "outer-active".to_string(),
        phases: Vec::new(),
        args_digest: "blake3:outer".to_string(),
    })];
    for index in 0..4 {
        let run_id = format!("newer-terminal-{index}");
        events.extend([
            WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
                run_id: run_id.clone(),
                resumed_from_run_id: None,
                name: run_id.clone(),
                phases: Vec::new(),
                args_digest: format!("blake3:{run_id}"),
            }),
            WorkflowEvent::RunEnd(WorkflowRunEndEvent {
                run_id,
                status: AgentStatus::Completed(None),
                terminal_reason: Some(WorkflowRunTerminalReason::Completed),
                spent: 0,
                total: Some(0),
            }),
        ]);
    }
    validate_reducer_fixture(&events)?;
    for event in events {
        append_rollout_item_to_path(&path, &RolloutItem::EventMsg(EventMsg::Workflow(event)))
            .await?;
    }

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.initialize_with_capabilities(
            ClientInfo {
                name: "workflow-resume-active-priority-test".to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: true,
                ..Default::default()
            }),
        ),
    )
    .await??;

    let resume_id = app_server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread_id.clone(),
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        read_resume_response_before_workflow(&mut app_server, RequestId::Integer(resume_id)),
    )
    .await??;

    let replay = timeout(
        DEFAULT_READ_TIMEOUT,
        read_workflow_replay(&mut app_server, /*expected_count*/ 7),
    )
    .await??;
    let lifecycle = replay
        .iter()
        .map(|notification| {
            (
                notification["method"].as_str().expect("method"),
                notification["params"]["runId"].as_str().expect("runId"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        lifecycle,
        vec![
            ("workflow/started", "outer-active"),
            ("workflow/started", "newer-terminal-1"),
            ("workflow/completed", "newer-terminal-1"),
            ("workflow/started", "newer-terminal-2"),
            ("workflow/completed", "newer-terminal-2"),
            ("workflow/started", "newer-terminal-3"),
            ("workflow/completed", "newer-terminal-3"),
        ]
    );

    let read_id = app_server
        .send_thread_read_request(ThreadReadParams {
            thread_id,
            include_turns: false,
        })
        .await?;
    timeout(
        DEFAULT_READ_TIMEOUT,
        app_server.read_stream_until_response_message(RequestId::Integer(read_id)),
    )
    .await??;
    assert_eq!(
        app_server
            .pending_notification_methods()
            .into_iter()
            .filter(|method| method.starts_with("workflow/"))
            .collect::<Vec<_>>(),
        Vec::<String>::new()
    );

    Ok(())
}

async fn read_resume_response_before_workflow(
    app_server: &mut TestAppServer,
    request_id: RequestId,
) -> Result<JSONRPCResponse> {
    loop {
        match app_server.read_next_message().await? {
            JSONRPCMessage::Response(response) if response.id == request_id => return Ok(response),
            JSONRPCMessage::Error(error) if error.id == request_id => {
                bail!("thread/resume failed before workflow replay: {error:?}");
            }
            JSONRPCMessage::Notification(notification)
                if notification.method.starts_with("workflow/") =>
            {
                bail!(
                    "workflow replay preceded thread/resume response: {}",
                    notification.method
                );
            }
            _ => {}
        }
    }
}

async fn append_workflow_fixture(path: &std::path::Path) -> Result<()> {
    // A completed run proves terminal lifecycle reconstruction. A separate active run keeps its
    // latest counter snapshot replayable without violating reducer invariants at RunEnd.
    let mut events = vec![
        WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            resumed_from_run_id: Some("run-paused-source".to_string()),
            name: "release-audit".to_string(),
            phases: vec!["build".to_string()],
            args_digest: "blake3:args".to_string(),
        }),
        WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            phase_index: 0,
            title: "build".to_string(),
        }),
        WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            group_id: 1,
            parent_node_id: None,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }),
        WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            group_id: 2,
            parent_node_id: Some(1),
            kind: WorkflowGroupKind::Pipeline,
            item_count: 1,
        }),
        WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            node_id: 3,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: Some(2),
            label: "completed".to_string(),
            phase: Some("build".to_string()),
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffort::Medium,
        }),
        WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            node_id: 3,
            attempt: 0,
            child_thread_id: "thread-completed-child".to_string(),
        }),
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            node_id: 3,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: token_usage(/*total_tokens*/ 10),
            tool_call_count: 1,
            duration_ms: 100,
        }),
        WorkflowEvent::AgentEnd(WorkflowAgentEndEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            node_id: 3,
            attempt: 0,
            last_attempt_reason: None,
            status: AgentStatus::Completed(Some("artifact ready".to_string())),
            token_usage: token_usage(/*total_tokens*/ 25),
            tool_call_count: 4,
            duration_ms: 400,
            returned_null: false,
        }),
        WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            group_id: 2,
            kind: WorkflowGroupKind::Pipeline,
            item_count: 1,
        }),
        WorkflowEvent::GroupEnd(WorkflowGroupEndEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            group_id: 1,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }),
        WorkflowEvent::PhaseEnd(WorkflowPhaseEndEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            phase_index: 0,
            title: "build".to_string(),
        }),
        WorkflowEvent::RunEnd(WorkflowRunEndEvent {
            run_id: COMPLETED_RUN_ID.to_string(),
            status: AgentStatus::Completed(Some("workflow done".to_string())),
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: 25,
            total: Some(50),
        }),
        WorkflowEvent::RunBegin(WorkflowRunBeginEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            resumed_from_run_id: None,
            name: "active-audit".to_string(),
            phases: vec!["inspect".to_string()],
            args_digest: "blake3:active-args".to_string(),
        }),
        WorkflowEvent::PhaseBegin(WorkflowPhaseBeginEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            phase_index: 0,
            title: "inspect".to_string(),
        }),
        WorkflowEvent::GroupBegin(WorkflowGroupBeginEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            group_id: 1,
            parent_node_id: None,
            kind: WorkflowGroupKind::Parallel,
            item_count: 1,
        }),
        WorkflowEvent::AgentBegin(WorkflowAgentBeginEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            node_id: 2,
            attempt: 0,
            last_attempt_reason: None,
            parent_node_id: Some(1),
            label: "still-running".to_string(),
            phase: Some("inspect".to_string()),
            model: "gpt-5.4".to_string(),
            effort: ReasoningEffort::High,
        }),
        WorkflowEvent::AgentBound(WorkflowAgentBoundEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            node_id: 2,
            attempt: 0,
            child_thread_id: "thread-active-child".to_string(),
        }),
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            node_id: 2,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: token_usage(/*total_tokens*/ 30),
            tool_call_count: 2,
            duration_ms: 100,
        }),
        WorkflowEvent::AgentUpdated(WorkflowAgentUpdatedEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            node_id: 2,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: token_usage(/*total_tokens*/ 42),
            tool_call_count: 3,
            duration_ms: 200,
        }),
    ];
    events.extend((0..=RETAINED_LOG_COUNT).map(|index| {
        WorkflowEvent::Log(WorkflowLogEvent {
            run_id: ACTIVE_RUN_ID.to_string(),
            message: format!("log-{index}"),
        })
    }));
    validate_reducer_fixture(&events)?;

    for event in events {
        append_rollout_item_to_path(path, &RolloutItem::EventMsg(EventMsg::Workflow(event)))
            .await?;
    }
    Ok(())
}

fn validate_reducer_fixture(events: &[WorkflowEvent]) -> Result<()> {
    let mut models = BTreeMap::<String, WorkflowRunModel>::new();
    for event in events {
        if let WorkflowEvent::RunBegin(begin) = event {
            let previous =
                models.insert(begin.run_id.clone(), WorkflowRunModel::from_event(event)?);
            assert!(previous.is_none(), "fixture run IDs must be unique");
            continue;
        }
        let run_id = workflow_run_id(event);
        models
            .get_mut(run_id)
            .ok_or_else(|| anyhow::anyhow!("event preceded RunBegin for {run_id}"))?
            .apply(event)?;
    }
    Ok(())
}

fn workflow_run_id(event: &WorkflowEvent) -> &str {
    match event {
        WorkflowEvent::RunBegin(event) => &event.run_id,
        WorkflowEvent::RunEnd(event) => &event.run_id,
        WorkflowEvent::PhaseBegin(event) => &event.run_id,
        WorkflowEvent::PhaseEnd(event) => &event.run_id,
        WorkflowEvent::GroupBegin(event) => &event.run_id,
        WorkflowEvent::GroupEnd(event) => &event.run_id,
        WorkflowEvent::AgentBegin(event) => &event.run_id,
        WorkflowEvent::AgentBound(event) => &event.run_id,
        WorkflowEvent::AgentUpdated(event) => &event.run_id,
        WorkflowEvent::AgentEnd(event) => &event.run_id,
        WorkflowEvent::Log(event) => &event.run_id,
    }
}

async fn read_workflow_replay(
    app_server: &mut TestAppServer,
    expected_count: usize,
) -> Result<Vec<Value>> {
    let mut replay = Vec::with_capacity(expected_count);
    while replay.len() < expected_count {
        let message = app_server.read_next_message().await?;
        let JSONRPCMessage::Notification(notification) = message else {
            bail!("unexpected message while reading workflow replay: {message:?}");
        };
        if !notification.method.starts_with("workflow/") {
            continue;
        }
        replay.push(normalize_observed_at(notification)?);
    }
    Ok(replay)
}

fn normalize_observed_at(notification: JSONRPCNotification) -> Result<Value> {
    let mut value = serde_json::to_value(notification)?;
    let params = value
        .get_mut("params")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("workflow notification params must be an object"))?;
    let observed_at = [
        "startedAt",
        "boundAt",
        "changedAt",
        "updatedAt",
        "completedAt",
        "emittedAt",
    ]
    .into_iter()
    .find_map(|field| params.remove(field))
    .ok_or_else(|| anyhow::anyhow!("workflow notification missing observation timestamp"))?;
    assert!(
        observed_at.as_i64().is_some_and(|timestamp| timestamp > 0),
        "workflow observation timestamp should be positive: {observed_at}"
    );
    Ok(value)
}

fn expected_workflow_replay(thread_id: &str) -> Vec<Value> {
    let mut replay = vec![
        json!({"method":"workflow/started","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"resumedFromRunId":"run-paused-source","name":"release-audit","phases":["build"],"argsDigest":"blake3:args"}}),
        json!({"method":"workflow/phase/changed","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"phaseIndex":0,"title":"build","status":"active"}}),
        json!({"method":"workflow/group/started","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"groupId":1,"parentNodeId":null,"kind":"parallel","itemCount":1}}),
        json!({"method":"workflow/group/started","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"groupId":2,"parentNodeId":1,"kind":"pipeline","itemCount":1}}),
        json!({"method":"workflow/agent/started","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"nodeId":3,"attempt":0,"lastAttemptReason":null,"parentNodeId":2,"label":"completed","phase":"build","model":"gpt-5.4","effort":"medium"}}),
        json!({"method":"workflow/agent/bound","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"nodeId":3,"attempt":0,"childThreadId":"thread-completed-child"}}),
        json!({"method":"workflow/agent/completed","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"nodeId":3,"attempt":0,"lastAttemptReason":null,"status":"completed","message":"artifact ready","tokenUsage":token_usage_json(/*total_tokens*/ 25),"toolCallCount":4,"durationMs":400,"returnedNull":false}}),
        json!({"method":"workflow/group/completed","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"groupId":2,"kind":"pipeline","itemCount":1}}),
        json!({"method":"workflow/group/completed","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"groupId":1,"kind":"parallel","itemCount":1}}),
        json!({"method":"workflow/phase/changed","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"phaseIndex":0,"title":"build","status":"completed"}}),
        json!({"method":"workflow/completed","params":{"threadId":thread_id,"runId":COMPLETED_RUN_ID,"status":"completed","message":"workflow done","terminalReason":"completed","spent":25,"total":50}}),
        json!({"method":"workflow/started","params":{"threadId":thread_id,"runId":ACTIVE_RUN_ID,"resumedFromRunId":null,"name":"active-audit","phases":["inspect"],"argsDigest":"blake3:active-args"}}),
        json!({"method":"workflow/phase/changed","params":{"threadId":thread_id,"runId":ACTIVE_RUN_ID,"phaseIndex":0,"title":"inspect","status":"active"}}),
        json!({"method":"workflow/group/started","params":{"threadId":thread_id,"runId":ACTIVE_RUN_ID,"groupId":1,"parentNodeId":null,"kind":"parallel","itemCount":1}}),
        json!({"method":"workflow/agent/started","params":{"threadId":thread_id,"runId":ACTIVE_RUN_ID,"nodeId":2,"attempt":0,"lastAttemptReason":null,"parentNodeId":1,"label":"still-running","phase":"inspect","model":"gpt-5.4","effort":"high"}}),
        json!({"method":"workflow/agent/bound","params":{"threadId":thread_id,"runId":ACTIVE_RUN_ID,"nodeId":2,"attempt":0,"childThreadId":"thread-active-child"}}),
        json!({"method":"workflow/agent/updated","params":{"threadId":thread_id,"runId":ACTIVE_RUN_ID,"nodeId":2,"attempt":0,"lastAttemptReason":null,"tokenUsage":token_usage_json(/*total_tokens*/ 42),"toolCallCount":3,"durationMs":200}}),
    ];
    replay.extend((1..=RETAINED_LOG_COUNT).map(|index| {
        json!({"method":"workflow/log","params":{"threadId":thread_id,"runId":ACTIVE_RUN_ID,"message":format!("log-{index}")}})
    }));
    replay
}

fn token_usage(total_tokens: i64) -> TokenUsage {
    TokenUsage {
        input_tokens: total_tokens - 5,
        cached_input_tokens: 2,
        output_tokens: 4,
        reasoning_output_tokens: 1,
        total_tokens,
    }
}

fn token_usage_json(total_tokens: i64) -> Value {
    json!({
        "totalTokens": total_tokens,
        "inputTokens": total_tokens - 5,
        "cachedInputTokens": 2,
        "outputTokens": 4,
        "reasoningOutputTokens": 1,
    })
}

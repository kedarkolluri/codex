use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::WorkflowAgentControlAction;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentUpdatedNotification;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowPauseDisposition;
use codex_app_server_protocol::WorkflowPauseParams;
use codex_app_server_protocol::WorkflowPauseResponse;
use codex_app_server_protocol::WorkflowResumeParams;
use codex_app_server_protocol::WorkflowResumeResponse;
use codex_app_server_protocol::WorkflowRunTerminalReason;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_features::Feature;
use core_test_support::TestTargetOs;
use core_test_support::responses;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

pub(super) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const PAUSE_UNAVAILABLE: &str = "workflow run is unavailable for pause";
const RESUME_UNAVAILABLE: &str = "workflow run is unavailable for resume";
pub(super) const AGENT_UNAVAILABLE: &str = "workflow agent is unavailable for control";
const SECRET_MARKER: &str = "CONTROL_PRIVATE_ARGS_5F25";

pub(super) fn write_config(codex_home: &Path, server_uri: &str) -> Result<()> {
    let features = BTreeMap::from([(Feature::Workflow, true)]);
    write_mock_responses_config_toml(
        codex_home,
        server_uri,
        &features,
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;
    let config_path = codex_home.join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        config_path,
        config.replace(
            "sandbox_mode = \"read-only\"",
            "sandbox_mode = \"danger-full-access\"",
        ),
    )?;
    Ok(())
}

fn workflow_path(codex_home: &Path, name: &str) -> std::path::PathBuf {
    codex_home
        .join("workflows")
        .join(format!("{name}.workflow.js"))
}

pub(super) fn write_workflow(codex_home: &Path, name: &str, body: &str) -> Result<()> {
    std::fs::create_dir_all(codex_home.join("workflows"))?;
    std::fs::write(
        workflow_path(codex_home, name),
        format!(
            "export const meta = {{ name: '{name}', description: 'control integration test', phases: ['control'] }};\nphase('control');\n{body}\n"
        ),
    )?;
    Ok(())
}

pub(super) fn blocking_command() -> &'static str {
    match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "tail -f /dev/null",
        TestTargetOs::Windows => "[System.Threading.ManualResetEvent]::new($false).WaitOne()",
    }
}

pub(super) fn completed_with_output_tokens(id: &str, output_tokens: i64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": 0,
                "input_tokens_details": null,
                "output_tokens": output_tokens,
                "output_tokens_details": null,
                "total_tokens": output_tokens
            }
        }
    })
}

pub(super) async fn read_response<T: DeserializeOwned>(
    mcp: &mut TestAppServer,
    request_id: i64,
) -> Result<T> {
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response::<T>(response)
}

pub(super) async fn read_error(mcp: &mut TestAppServer, request_id: i64) -> Result<JSONRPCError> {
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await?
}

pub(super) async fn start_thread(mcp: &mut TestAppServer) -> Result<String> {
    let cwd = mcp.auto_env()?.cwd().clone().into_path_buf();
    let request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    Ok(read_response::<ThreadStartResponse>(mcp, request_id)
        .await?
        .thread
        .id)
}

pub(super) async fn start_workflow(
    mcp: &mut TestAppServer,
    thread_id: &str,
    name: &str,
    args: Option<Value>,
) -> Result<String> {
    let request_id = mcp
        .send_workflow_start_request(WorkflowStartParams {
            thread_id: thread_id.to_string(),
            name: name.to_string(),
            args,
        })
        .await?;
    Ok(read_response::<WorkflowStartResponse>(mcp, request_id)
        .await?
        .run_id)
}

fn notification_params<T: DeserializeOwned>(notification: &JSONRPCNotification) -> Result<T> {
    let params = notification
        .params
        .clone()
        .with_context(|| format!("{} notification has no params", notification.method))?;
    serde_json::from_value(params)
        .with_context(|| format!("deserialize {} notification", notification.method))
}

pub(super) async fn wait_for_blocked_agent(
    mcp: &mut TestAppServer,
    run_id: &str,
    expected_attempt: u32,
    expected_tool_call_count: u64,
) -> Result<(u64, u32)> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
                continue;
            };
            if notification.method != "workflow/agent/updated" {
                continue;
            }
            let updated: WorkflowAgentUpdatedNotification = notification_params(&notification)?;
            if updated.run_id == run_id
                && updated.attempt == expected_attempt
                && updated.tool_call_count == expected_tool_call_count
            {
                return Ok::<_, anyhow::Error>((updated.node_id, updated.attempt));
            }
        }
    })
    .await?
}

pub(super) async fn wait_for_completed(
    mcp: &mut TestAppServer,
    run_id: &str,
) -> Result<WorkflowCompletedNotification> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
                continue;
            };
            if notification.method != "workflow/completed" {
                continue;
            }
            let completed: WorkflowCompletedNotification = notification_params(&notification)?;
            if completed.run_id == run_id {
                return Ok::<_, anyhow::Error>(completed);
            }
        }
    })
    .await?
}

async fn wait_for_resumed_completion(
    mcp: &mut TestAppServer,
    run_id: &str,
    source_run_id: &str,
) -> Result<WorkflowCompletedNotification> {
    timeout(DEFAULT_TIMEOUT, async {
        let mut saw_lineage = false;
        loop {
            let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
                continue;
            };
            match notification.method.as_str() {
                "workflow/started" => {
                    let started: WorkflowStartedNotification = notification_params(&notification)?;
                    if started.run_id == run_id {
                        assert_eq!(started.resumed_from_run_id.as_deref(), Some(source_run_id));
                        saw_lineage = true;
                    }
                }
                "workflow/completed" => {
                    let completed: WorkflowCompletedNotification =
                        notification_params(&notification)?;
                    if completed.run_id == run_id {
                        assert!(
                            saw_lineage,
                            "resumed run must announce lineage before completion"
                        );
                        return Ok::<_, anyhow::Error>(completed);
                    }
                }
                _ => {}
            }
        }
    })
    .await?
}

async fn wait_for_workflows_changed(mcp: &mut TestAppServer) -> Result<()> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let JSONRPCMessage::Notification(notification) = mcp.read_next_message().await? else {
                continue;
            };
            if notification.method == "workflows/changed" {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await?
}

pub(super) fn assert_bounded_error(error: JSONRPCError, expected: &str) {
    assert_eq!(error.error.code, -32600);
    assert_eq!(error.error.message, expected);
    assert_eq!(error.error.data, None);
}

#[tokio::test]
async fn workflow_controls_require_experimental_api_and_reject_secret_fields() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    let initialized = mcp
        .initialize_with_capabilities(
            ClientInfo {
                name: DEFAULT_CLIENT_NAME.to_string(),
                title: None,
                version: "0.1.0".to_string(),
            },
            Some(InitializeCapabilities {
                experimental_api: false,
                ..Default::default()
            }),
        )
        .await?;
    assert!(matches!(initialized, JSONRPCMessage::Response(_)));
    let thread_id = "00000000-0000-4000-8000-000000000001";
    let run_id = "0198d9a4-9c96-7e11-8b05-f7aa47280743";
    let requests = [
        (
            mcp.send_workflow_pause_request(WorkflowPauseParams {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
            })
            .await?,
            "workflow/pause requires experimentalApi capability",
        ),
        (
            mcp.send_workflow_resume_request(WorkflowResumeParams {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
            })
            .await?,
            "workflow/resume requires experimentalApi capability",
        ),
        (
            mcp.send_workflow_agent_control_request(WorkflowAgentControlParams {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
                node_id: 1,
                attempt: 0,
                action: WorkflowAgentControlAction::Skip,
            })
            .await?,
            "workflow/agent/control requires experimentalApi capability",
        ),
    ];
    for (request_id, expected) in requests {
        assert_bounded_error(read_error(&mut mcp, request_id).await?, expected);
    }

    drop(mcp);
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    mcp.initialize().await?;
    for request_id in [
        mcp.send_workflow_pause_raw_request(json!({
            "threadId":thread_id,"runId":run_id,"source":SECRET_MARKER
        }))
        .await?,
        mcp.send_workflow_resume_raw_request(json!({
            "threadId":thread_id,"runId":run_id,"args":{"secret":SECRET_MARKER}
        }))
        .await?,
        mcp.send_workflow_agent_control_raw_request(json!({
            "threadId":thread_id,"runId":run_id,"nodeId":1,"attempt":0,
            "action":"skip","path":SECRET_MARKER
        }))
        .await?,
    ] {
        let error = read_error(&mut mcp, request_id).await?;
        assert_eq!(error.error.code, -32600);
        assert!(error.error.message.contains("unknown field"));
        assert!(!error.error.message.contains(SECRET_MARKER));
        assert_eq!(error.error.data, None);
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_use_quiescent_private_immutable_checkpoint() -> Result<()> {
    let server = responses::start_mock_server().await;
    let tool_args = serde_json::to_string(&json!({
        "command": blocking_command(),
        "login": false,
        "timeout_ms": 10_000,
    }))?;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("control-pause-initial"),
                responses::ev_function_call("pause-shell", "shell_command", &tool_args),
                completed_with_output_tokens("control-pause-initial", 5),
            ]),
            responses::sse(vec![
                responses::ev_response_created("control-resumed"),
                responses::ev_assistant_message("control-resumed-message", "resumed-original"),
                completed_with_output_tokens("control-resumed", 7),
            ]),
        ],
    )
    .await;
    let codex_home = TempDir::new()?;
    write_config(codex_home.path(), &server.uri())?;
    write_workflow(
        codex_home.path(),
        "checkpoint-control",
        "await agent(`ORIGINAL:${args.secret}`, { label: 'checkpoint-child', phase: 'control' });",
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let owner_thread_id = start_thread(&mut mcp).await?;
    let other_thread_id = start_thread(&mut mcp).await?;
    let source_run_id = start_workflow(
        &mut mcp,
        &owner_thread_id,
        "checkpoint-control",
        Some(json!({"secret":SECRET_MARKER})),
    )
    .await?;
    wait_for_blocked_agent(
        &mut mcp,
        &source_run_id,
        /*expected_attempt*/ 0,
        /*expected_tool_call_count*/ 1,
    )
    .await?;

    let wrong_thread_pause = mcp
        .send_workflow_pause_request(WorkflowPauseParams {
            thread_id: other_thread_id.clone(),
            run_id: source_run_id.clone(),
        })
        .await?;
    assert_bounded_error(
        read_error(&mut mcp, wrong_thread_pause).await?,
        PAUSE_UNAVAILABLE,
    );

    let first_pause = mcp
        .send_workflow_pause_request(WorkflowPauseParams {
            thread_id: owner_thread_id.clone(),
            run_id: source_run_id.clone(),
        })
        .await?;
    let duplicate_pause = mcp
        .send_workflow_pause_request(WorkflowPauseParams {
            thread_id: owner_thread_id.clone(),
            run_id: source_run_id.clone(),
        })
        .await?;
    let mut dispositions = vec![
        read_response::<WorkflowPauseResponse>(&mut mcp, first_pause)
            .await?
            .disposition,
        read_response::<WorkflowPauseResponse>(&mut mcp, duplicate_pause)
            .await?
            .disposition,
    ];
    dispositions.sort_by_key(|disposition| match disposition {
        WorkflowPauseDisposition::Applied => 0,
        WorkflowPauseDisposition::AlreadyRequested => 1,
    });
    assert_eq!(
        dispositions,
        vec![
            WorkflowPauseDisposition::Applied,
            WorkflowPauseDisposition::AlreadyRequested,
        ]
    );
    let paused = wait_for_completed(&mut mcp, &source_run_id).await?;
    assert_eq!(
        paused.terminal_reason,
        Some(WorkflowRunTerminalReason::Paused)
    );
    let run_dir = codex_home
        .path()
        .join("workflows/runs")
        .join(&source_run_id);
    let meta: Value = serde_json::from_slice(&std::fs::read(run_dir.join("meta.json"))?)?;
    let progress: Value = serde_json::from_slice(&std::fs::read(run_dir.join("progress.json"))?)?;
    assert_eq!(meta["status"], "paused");
    assert_eq!(
        (progress["state"].clone(), progress["status"].clone()),
        (json!("terminal"), json!("paused"))
    );

    std::fs::remove_file(workflow_path(codex_home.path(), "checkpoint-control"))?;
    wait_for_workflows_changed(&mut mcp).await?;
    let list_request = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: owner_thread_id.clone(),
            cursor: None,
            limit: None,
        })
        .await?;
    assert_eq!(
        read_response::<WorkflowListResponse>(&mut mcp, list_request)
            .await?
            .data,
        Vec::new()
    );

    let uppercase_source_run_id = source_run_id.to_uppercase();
    for (thread_id, run_id) in [
        (other_thread_id.as_str(), source_run_id.as_str()),
        (
            owner_thread_id.as_str(),
            "0198d9a4-9c96-7e11-8b05-f7aa47280743",
        ),
        (owner_thread_id.as_str(), uppercase_source_run_id.as_str()),
    ] {
        let request_id = mcp
            .send_workflow_resume_request(WorkflowResumeParams {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
            })
            .await?;
        assert_bounded_error(read_error(&mut mcp, request_id).await?, RESUME_UNAVAILABLE);
    }

    let first_resume = mcp
        .send_workflow_resume_request(WorkflowResumeParams {
            thread_id: owner_thread_id.clone(),
            run_id: source_run_id.clone(),
        })
        .await?;
    let duplicate_resume = mcp
        .send_workflow_resume_request(WorkflowResumeParams {
            thread_id: owner_thread_id,
            run_id: source_run_id.clone(),
        })
        .await?;
    let first = read_response::<WorkflowResumeResponse>(&mut mcp, first_resume).await?;
    let duplicate = read_response::<WorkflowResumeResponse>(&mut mcp, duplicate_resume).await?;
    assert_eq!(first, duplicate);
    assert_eq!(serde_json::to_value(&first)?, json!({"runId":first.run_id}));
    let completed = wait_for_resumed_completion(&mut mcp, &first.run_id, &source_run_id).await?;
    assert_eq!(
        completed.terminal_reason,
        Some(WorkflowRunTerminalReason::Completed)
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.body_contains_text(&format!("ORIGINAL:{SECRET_MARKER}")))
    );
    Ok(())
}

#[tokio::test]
async fn malformed_and_unknown_pause_targets_share_one_bounded_error() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(codex_home.path(), &server.uri())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;
    for run_id in [
        "not-a-uuid",
        "0198D9A4-9C96-7E11-8B05-F7AA47280743",
        "0198d9a4-9c96-7e11-8b05-f7aa47280743",
    ] {
        let request_id = mcp
            .send_workflow_pause_request(WorkflowPauseParams {
                thread_id: thread_id.clone(),
                run_id: run_id.to_string(),
            })
            .await?;
        assert_bounded_error(read_error(&mut mcp, request_id).await?, PAUSE_UNAVAILABLE);
    }
    Ok(())
}

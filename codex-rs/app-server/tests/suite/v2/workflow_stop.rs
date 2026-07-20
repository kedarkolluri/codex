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
use codex_app_server_protocol::WorkflowAgentUpdatedNotification;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_app_server_protocol::WorkflowStopDisposition;
use codex_app_server_protocol::WorkflowStopParams;
use codex_app_server_protocol::WorkflowStopResponse;
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

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_PROMPT: &str = "APP_SERVER_WORKFLOW_STOP_CHILD";
const TOOL_CALL_ID: &str = "app-workflow-stop-tool";
const INACTIVE_ERROR: &str = "workflow run is not active for this thread";

fn write_config(codex_home: &Path, server_uri: &str) -> Result<()> {
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

fn write_workflow(codex_home: &Path) -> Result<()> {
    let workflows = codex_home.join("workflows");
    std::fs::create_dir_all(&workflows)?;
    std::fs::write(
        workflows.join("stop-target.workflow.js"),
        format!(
            r#"export const meta = {{ name: 'stop-target', description: 'stop integration test', phases: ['verify'] }};
phase('verify');
await agent('{CHILD_PROMPT}', {{ label: 'stop-child', phase: 'verify' }});
"#,
        ),
    )?;
    Ok(())
}

fn blocking_command() -> &'static str {
    match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "tail -f /dev/null",
        TestTargetOs::Windows => "[System.Threading.ManualResetEvent]::new($false).WaitOne()",
    }
}

fn completed_with_output_tokens(id: &str, output_tokens: i64) -> Value {
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

async fn read_response<T: DeserializeOwned>(mcp: &mut TestAppServer, request_id: i64) -> Result<T> {
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response::<T>(response)
}

async fn read_error(mcp: &mut TestAppServer, request_id: i64) -> Result<JSONRPCError> {
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await?
}

async fn start_thread(mcp: &mut TestAppServer) -> Result<String> {
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

async fn stop_workflow(mcp: &mut TestAppServer, thread_id: &str, run_id: &str) -> Result<i64> {
    mcp.send_workflow_stop_request(WorkflowStopParams {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
    })
    .await
}

fn notification_params<T: DeserializeOwned>(notification: &JSONRPCNotification) -> Result<T> {
    let params = notification
        .params
        .clone()
        .with_context(|| format!("{} notification has no params", notification.method))?;
    serde_json::from_value(params)
        .with_context(|| format!("deserialize {} notification", notification.method))
}

async fn wait_for_blocked_child(mcp: &mut TestAppServer, run_id: &str) -> Result<()> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let message = mcp.read_next_message().await?;
            let JSONRPCMessage::Notification(notification) = message else {
                continue;
            };
            if notification.method != "workflow/agent/updated"
                || notification
                    .params
                    .as_ref()
                    .and_then(|params| params.get("runId"))
                    .and_then(Value::as_str)
                    != Some(run_id)
            {
                continue;
            }
            let updated: WorkflowAgentUpdatedNotification = notification_params(&notification)?;
            if updated.tool_call_count == 1 {
                return Ok::<_, anyhow::Error>(());
            }
        }
    })
    .await?
}

#[tokio::test]
async fn workflow_stop_requires_experimental_api_capability() -> Result<()> {
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
    let JSONRPCMessage::Response(_) = initialized else {
        anyhow::bail!("expected initialize response, got {initialized:?}");
    };

    let request_id = stop_workflow(
        &mut mcp,
        "00000000-0000-4000-8000-000000000001",
        "0198d9a4-9c96-7e11-8b05-f7aa47280743",
    )
    .await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        "workflow/stop requires experimentalApi capability"
    );
    assert_eq!(error.data, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_stop_is_targeted_non_leaking_and_joins_duplicate_cleanup() -> Result<()> {
    let server = responses::start_mock_server().await;
    let tool_args = serde_json::to_string(&json!({
        "command": blocking_command(),
        "login": false,
        "timeout_ms": 10_000,
    }))?;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("app-workflow-stop-child"),
            responses::ev_function_call(TOOL_CALL_ID, "shell_command", &tool_args),
            completed_with_output_tokens("app-workflow-stop-child", 7),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    write_config(codex_home.path(), &server.uri())?;
    write_workflow(codex_home.path())?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let owner_thread_id = start_thread(&mut mcp).await?;
    let other_thread_id = start_thread(&mut mcp).await?;

    let start_request_id = mcp
        .send_workflow_start_request(WorkflowStartParams {
            thread_id: owner_thread_id.clone(),
            name: "stop-target".to_string(),
            args: None,
        })
        .await?;
    let WorkflowStartResponse { run_id } =
        read_response::<WorkflowStartResponse>(&mut mcp, start_request_id).await?;
    wait_for_blocked_child(&mut mcp, &run_id).await?;
    assert!(
        response_mock
            .single_request()
            .body_contains_text(CHILD_PROMPT)
    );

    let unknown_run_id = "0198d9a4-9c96-7e11-8b05-f7aa47280743";
    let wrong_thread_request = stop_workflow(&mut mcp, &other_thread_id, &run_id).await?;
    let unknown_run_request = stop_workflow(&mut mcp, &owner_thread_id, unknown_run_id).await?;
    for request_id in [wrong_thread_request, unknown_run_request] {
        let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
        assert_eq!(error.code, -32600);
        assert_eq!(error.message, INACTIVE_ERROR);
        assert_eq!(error.data, None);
    }

    // Stop is intentionally not request-serialized: both callers enter the
    // task manager while cleanup is in flight and wait for the same completion.
    let first_stop_request = stop_workflow(&mut mcp, &owner_thread_id, &run_id).await?;
    let duplicate_stop_request = stop_workflow(&mut mcp, &owner_thread_id, &run_id).await?;
    let first: WorkflowStopResponse = read_response(&mut mcp, first_stop_request).await?;
    let duplicate: WorkflowStopResponse = read_response(&mut mcp, duplicate_stop_request).await?;
    let mut dispositions = vec![first.disposition, duplicate.disposition];
    dispositions.sort_by_key(|disposition| match disposition {
        WorkflowStopDisposition::Applied => 0,
        WorkflowStopDisposition::AlreadyRequested => 1,
    });
    assert_eq!(
        dispositions,
        vec![
            WorkflowStopDisposition::Applied,
            WorkflowStopDisposition::AlreadyRequested,
        ]
    );

    // Both responses are delayed until core's existing cleanup path has
    // terminalized the run durably.
    let progress: Value = serde_json::from_slice(&std::fs::read(
        codex_home
            .path()
            .join("workflows/runs")
            .join(&run_id)
            .join("progress.json"),
    )?)?;
    assert_eq!(
        (progress["state"].clone(), progress["status"].clone()),
        (json!("terminal"), json!("stopped"))
    );

    let inactive_request = stop_workflow(&mut mcp, &owner_thread_id, &run_id).await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, inactive_request).await?;
    assert_eq!(error.code, -32600);
    assert_eq!(error.message, INACTIVE_ERROR);
    assert_eq!(error.data, None);
    Ok(())
}

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::WorkflowReadParams;
use codex_app_server_protocol::WorkflowReadResponse;
use codex_app_server_protocol::WorkflowRunStatus;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const WORKFLOW_RUN_UNAVAILABLE_MESSAGE: &str = "workflow run is unavailable for this thread";

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
    let workflows = codex_home.join("workflows");
    std::fs::create_dir_all(&workflows)?;
    std::fs::write(
        workflows.join("background.workflow.js"),
        concat!(
            "// @exec: {\"yield_time_ms\": 60000}\n",
            "export const meta = { name: 'background', description: 'restart test' };\n",
            "await new Promise(() => {});\n",
        ),
    )?;
    Ok(())
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

async fn read_workflow(mcp: &mut TestAppServer, thread_id: &str, run_id: &str) -> Result<i64> {
    mcp.send_workflow_read_request(WorkflowReadParams {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
    })
    .await
}

#[tokio::test]
async fn workflow_read_reports_reconciled_status_after_app_server_restart() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(codex_home.path(), &server.uri())?;

    let mut first_mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, first_mcp.initialize()).await??;
    let thread_id = start_thread(&mut first_mcp).await?;
    let start_id = first_mcp
        .send_workflow_start_request(WorkflowStartParams {
            thread_id: thread_id.clone(),
            name: "background".to_string(),
            args: None,
        })
        .await?;
    let WorkflowStartResponse { run_id } =
        read_response::<WorkflowStartResponse>(&mut first_mcp, start_id).await?;

    let running_id = read_workflow(&mut first_mcp, &thread_id, &run_id).await?;
    assert_eq!(
        read_response::<WorkflowReadResponse>(&mut first_mcp, running_id).await?,
        WorkflowReadResponse {
            run_id: run_id.clone(),
            status: WorkflowRunStatus::Running,
        }
    );

    let exit_status = first_mcp.force_kill_and_wait().await?;
    assert!(!exit_status.success());
    drop(first_mcp);

    let mut second_mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, second_mcp.initialize()).await??;

    let read_id = read_workflow(&mut second_mcp, &thread_id, &run_id).await?;
    assert_eq!(
        read_response::<WorkflowReadResponse>(&mut second_mcp, read_id).await?,
        WorkflowReadResponse {
            run_id: run_id.clone(),
            status: WorkflowRunStatus::Failed,
        }
    );

    let wrong_thread_id = uuid::Uuid::now_v7().to_string();
    let wrong_owner_id = read_workflow(&mut second_mcp, &wrong_thread_id, &run_id).await?;
    let unknown_run_id = uuid::Uuid::now_v7().to_string();
    let unknown_id = read_workflow(&mut second_mcp, &thread_id, &unknown_run_id).await?;
    let malformed_id = read_workflow(&mut second_mcp, &thread_id, "not-a-run-id").await?;
    let wrong_owner = read_error(&mut second_mcp, wrong_owner_id).await?;
    let unknown = read_error(&mut second_mcp, unknown_id).await?;
    let malformed = read_error(&mut second_mcp, malformed_id).await?;
    assert_eq!(wrong_owner.error.message, WORKFLOW_RUN_UNAVAILABLE_MESSAGE);
    assert_eq!(unknown.error, wrong_owner.error);
    assert_eq!(malformed.error, wrong_owner.error);

    let meta_path = codex_home
        .path()
        .join("workflows/runs")
        .join(&run_id)
        .join("meta.json");
    let mut meta: serde_json::Value = serde_json::from_slice(&std::fs::read(&meta_path)?)?;
    meta.as_object_mut()
        .expect("workflow metadata is an object")
        .remove("owner_thread_id");
    std::fs::write(meta_path, serde_json::to_vec(&meta)?)?;
    let ownerless_id = read_workflow(&mut second_mcp, &thread_id, &run_id).await?;
    let ownerless = read_error(&mut second_mcp, ownerless_id).await?;
    assert_eq!(ownerless.error, wrong_owner.error);
    Ok(())
}

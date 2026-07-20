use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_features::Feature;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const WORKFLOW_START_MAX_NAME_BYTES: usize = 256;
const WORKFLOW_START_MAX_ARGS_BYTES: usize = 32 * 1024;

fn write_config(codex_home: &Path, server_uri: &str, workflow_enabled: bool) -> Result<()> {
    let features = BTreeMap::from([(Feature::Workflow, workflow_enabled)]);
    write_mock_responses_config_toml(
        codex_home,
        server_uri,
        &features,
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
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

async fn start_workflow(
    mcp: &mut TestAppServer,
    thread_id: &str,
    name: &str,
    args: Option<Value>,
) -> Result<i64> {
    mcp.send_workflow_start_request(WorkflowStartParams {
        thread_id: thread_id.to_string(),
        name: name.to_string(),
        args,
    })
    .await
}

#[tokio::test]
async fn workflow_start_rejects_unknown_thread() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    let thread_id = "00000000-0000-4000-8000-000000000001";
    let request_id = start_workflow(&mut mcp, thread_id, "missing", /*args*/ None).await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
    assert_eq!(error.message, format!("thread not found: {thread_id}"));
    Ok(())
}

#[tokio::test]
async fn workflow_start_requires_experimental_api_capability() -> Result<()> {
    let codex_home = TempDir::new()?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    let initialized = mcp
        .initialize_with_capabilities(
            ClientInfo {
                name: "workflow-start-test".to_string(),
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

    let request_id = start_workflow(
        &mut mcp,
        "00000000-0000-4000-8000-000000000001",
        "missing",
        /*args*/ None,
    )
    .await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        "workflow/start requires experimentalApi capability"
    );
    assert_eq!(error.data, None);
    Ok(())
}

#[tokio::test]
async fn workflow_start_rejects_oversized_inputs_before_creating_run_artifacts() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .without_auto_env()
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = "00000000-0000-4000-8000-000000000001";

    let oversized_name = "n".repeat(WORKFLOW_START_MAX_NAME_BYTES + 1);
    let request_id = start_workflow(&mut mcp, thread_id, &oversized_name, /*args*/ None).await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        format!("workflow name exceeds the {WORKFLOW_START_MAX_NAME_BYTES}-byte limit")
    );
    assert_eq!(error.data, None);

    let oversized_args = Value::String("a".repeat(WORKFLOW_START_MAX_ARGS_BYTES));
    let request_id = start_workflow(&mut mcp, thread_id, "bounded", Some(oversized_args)).await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        format!("workflow args exceed the {WORKFLOW_START_MAX_ARGS_BYTES}-byte execution cap")
    );
    assert_eq!(error.data, None);
    let runs_root = codex_home.path().join("workflows/runs");
    let run_artifacts = match std::fs::read_dir(&runs_root) {
        Ok(entries) => entries.collect::<std::io::Result<Vec<_>>>()?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    assert!(
        run_artifacts.is_empty(),
        "rejected workflow/start inputs must not create run artifacts: {run_artifacts:#?}"
    );
    Ok(())
}

#[tokio::test]
async fn workflow_start_rejects_unknown_saved_name() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;

    let request_id = start_workflow(&mut mcp, &thread_id, "missing", /*args*/ None).await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
    assert_eq!(error.message, "saved workflow not found: missing");
    Ok(())
}

#[tokio::test]
async fn workflow_start_rejects_feature_disabled_for_thread() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ false,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;

    let request_id = start_workflow(&mut mcp, &thread_id, "missing", /*args*/ None).await?;
    let JSONRPCError { error, .. } = read_error(&mut mcp, request_id).await?;
    assert_eq!(
        error.message,
        format!("workflow feature is disabled for thread {thread_id}")
    );
    Ok(())
}

#[tokio::test]
async fn workflow_start_returns_after_durable_initialization() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let workflows = codex_home.path().join("workflows");
    std::fs::create_dir_all(&workflows)?;
    std::fs::write(
        workflows.join("background.workflow.js"),
        concat!(
            "// @exec: {\"yield_time_ms\": 60000}\n",
            "export const meta = { name: 'background', description: 'integration test' };\n",
            "await new Promise(() => {});\n",
        ),
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;

    let request_id = start_workflow(
        &mut mcp,
        &thread_id,
        "background",
        Some(serde_json::json!({"target": "main"})),
    )
    .await?;
    let response: JSONRPCResponse = timeout(
        Duration::from_secs(5),
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let response = to_response::<WorkflowStartResponse>(response)?;
    uuid::Uuid::parse_str(&response.run_id)?;

    let run_dir = codex_home
        .path()
        .join("workflows/runs")
        .join(&response.run_id);
    assert!(run_dir.join("script.js").is_file());
    assert!(run_dir.join("journal.jsonl").is_file());
    let meta: Value = serde_json::from_slice(&std::fs::read(run_dir.join("meta.json"))?)?;
    assert_eq!(meta["run_id"], response.run_id);
    assert_eq!(meta["name"], "background");
    assert!(meta.get("status").is_none());
    Ok(())
}

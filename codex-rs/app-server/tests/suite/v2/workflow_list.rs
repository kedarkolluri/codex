use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
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
use codex_app_server_protocol::WorkflowListParams;
use codex_app_server_protocol::WorkflowListResponse;
use codex_app_server_protocol::WorkflowMetadata;
use codex_app_server_protocol::WorkflowScope;
use codex_core::config::set_project_trust_level;
use codex_features::Feature;
use codex_protocol::config_types::TrustLevel;
use codex_utils_path_uri::LegacyAppPathString;
use core_test_support::responses;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const WATCHER_TIMEOUT: Duration = Duration::from_secs(20);

fn workflow_source(name: &str, description: &str, phases: &[&str]) -> String {
    let phases = phases
        .iter()
        .map(|phase| format!("'{phase}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "export const meta = {{ name: '{name}', description: '{description}', phases: [{phases}] }};\n"
    )
}

fn write_workflow(
    root: &Path,
    filename: &str,
    name: &str,
    description: &str,
    phases: &[&str],
) -> Result<()> {
    std::fs::create_dir_all(root)?;
    std::fs::write(
        root.join(filename),
        workflow_source(name, description, phases),
    )?;
    Ok(())
}

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

fn write_project_workflow_feature(
    codex_home: &Path,
    workspace: &Path,
    workflow_enabled: bool,
) -> Result<()> {
    let project_config_dir = workspace.join(".codex");
    std::fs::create_dir_all(&project_config_dir)?;
    std::fs::write(
        project_config_dir.join("config.toml"),
        format!("[features]\nworkflow = {workflow_enabled}\n"),
    )?;
    set_project_trust_level(codex_home, workspace, TrustLevel::Trusted)?;
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

async fn start_thread(mcp: &mut TestAppServer, cwd: &Path) -> Result<String> {
    let request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let response = read_response::<ThreadStartResponse>(mcp, request_id).await?;
    Ok(response.thread.id)
}

async fn list_workflows(
    mcp: &mut TestAppServer,
    thread_id: &str,
    cursor: Option<String>,
    limit: Option<u32>,
) -> Result<WorkflowListResponse> {
    let request_id = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: thread_id.to_string(),
            cursor,
            limit,
        })
        .await?;
    read_response(mcp, request_id).await
}

#[tokio::test]
async fn workflow_list_applies_precedence_and_paginates_deterministically() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "saved workflow fixtures are host-local in this integration test"
    );

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;

    let codex_root = codex_home.path().join("workflows");
    write_workflow(
        &codex_root,
        "shared.workflow.js",
        "shared",
        "codex copy",
        &["codex phase"],
    )?;
    write_workflow(
        &codex_root,
        "zulu.workflow.js",
        "zulu",
        "last workflow",
        &[],
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let thread_cwd = mcp.auto_env()?.cwd().clone().into_path_buf();
    let project_root = thread_cwd.join(".codex").join("workflows");
    write_workflow(
        &project_root,
        "alpha.workflow.js",
        "alpha",
        "first workflow",
        &["plan", "run"],
    )?;
    write_workflow(
        &project_root,
        "shared.workflow.js",
        "shared",
        "project copy",
        &["project phase"],
    )?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp, &thread_cwd).await?;

    let first = list_workflows(
        &mut mcp,
        &thread_id,
        /*cursor*/ None,
        /*limit*/ Some(2),
    )
    .await?;
    assert_eq!(
        first,
        WorkflowListResponse {
            data: vec![
                WorkflowMetadata {
                    name: "alpha".to_string(),
                    description: "first workflow".to_string(),
                    phases: vec!["plan".to_string(), "run".to_string()],
                    scope: WorkflowScope::Project,
                    path: LegacyAppPathString::from_path(&project_root.join("alpha.workflow.js"),),
                },
                WorkflowMetadata {
                    name: "shared".to_string(),
                    description: "project copy".to_string(),
                    phases: vec!["project phase".to_string()],
                    scope: WorkflowScope::Project,
                    path: LegacyAppPathString::from_path(&project_root.join("shared.workflow.js"),),
                },
            ],
            next_cursor: Some("2".to_string()),
        }
    );

    let second = list_workflows(
        &mut mcp,
        &thread_id,
        first.next_cursor,
        /*limit*/ Some(2),
    )
    .await?;
    assert_eq!(
        second,
        WorkflowListResponse {
            data: vec![WorkflowMetadata {
                name: "zulu".to_string(),
                description: "last workflow".to_string(),
                phases: Vec::new(),
                scope: WorkflowScope::CodexHome,
                path: LegacyAppPathString::from_path(&codex_root.join("zulu.workflow.js")),
            }],
            next_cursor: None,
        }
    );

    for (cursor, expected_message) in [
        ("invalid", "invalid cursor: invalid"),
        ("99", "cursor 99 exceeds total workflows 3"),
    ] {
        let request_id = mcp
            .send_workflow_list_request(WorkflowListParams {
                thread_id: thread_id.clone(),
                cursor: Some(cursor.to_string()),
                limit: None,
            })
            .await?;
        let JSONRPCError { error, .. } = timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
        )
        .await??;
        assert_eq!(error.message, expected_message);
    }

    Ok(())
}

#[tokio::test]
async fn workflow_list_enforces_hard_page_limit() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let codex_root = codex_home.path().join("workflows");
    for index in 0..101 {
        let name = format!("workflow-{index:03}");
        write_workflow(
            &codex_root,
            &format!("{name}.workflow.js"),
            &name,
            &format!("description {index}"),
            &[],
        )?;
    }

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let thread_cwd = mcp.auto_env()?.cwd().clone().into_path_buf();
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp, &thread_cwd).await?;
    let response = list_workflows(
        &mut mcp,
        &thread_id,
        /*cursor*/ None,
        /*limit*/ Some(u32::MAX),
    )
    .await?;

    let expected_data = (0..100)
        .map(|index| {
            let name = format!("workflow-{index:03}");
            WorkflowMetadata {
                description: format!("description {index}"),
                path: LegacyAppPathString::from_path(
                    &codex_root.join(format!("{name}.workflow.js")),
                ),
                name,
                phases: Vec::new(),
                scope: WorkflowScope::CodexHome,
            }
        })
        .collect();
    assert_eq!(
        response,
        WorkflowListResponse {
            data: expected_data,
            next_cursor: Some("100".to_string()),
        }
    );

    Ok(())
}

#[tokio::test]
async fn workflow_list_observes_watcher_cache_invalidation() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let codex_root = codex_home.path().join("workflows");
    write_workflow(
        &codex_root,
        "alpha.workflow.js",
        "alpha",
        "first version",
        &[],
    )?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    let thread_cwd = mcp.auto_env()?.cwd().clone().into_path_buf();
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp, &thread_cwd).await?;
    let first = list_workflows(
        &mut mcp, &thread_id, /*cursor*/ None, /*limit*/ None,
    )
    .await?;
    assert_eq!(
        first,
        WorkflowListResponse {
            data: vec![WorkflowMetadata {
                name: "alpha".to_string(),
                description: "first version".to_string(),
                phases: Vec::new(),
                scope: WorkflowScope::CodexHome,
                path: LegacyAppPathString::from_path(&codex_root.join("alpha.workflow.js")),
            }],
            next_cursor: None,
        }
    );

    write_workflow(
        &codex_root,
        "bravo.workflow.js",
        "bravo",
        "added after cache warmup",
        &[],
    )?;
    timeout(
        WATCHER_TIMEOUT,
        mcp.read_stream_until_notification_message("workflows/changed"),
    )
    .await??;

    let refreshed = list_workflows(
        &mut mcp, &thread_id, /*cursor*/ None, /*limit*/ None,
    )
    .await?;
    assert_eq!(
        refreshed,
        WorkflowListResponse {
            data: vec![
                WorkflowMetadata {
                    name: "alpha".to_string(),
                    description: "first version".to_string(),
                    phases: Vec::new(),
                    scope: WorkflowScope::CodexHome,
                    path: LegacyAppPathString::from_path(&codex_root.join("alpha.workflow.js"),),
                },
                WorkflowMetadata {
                    name: "bravo".to_string(),
                    description: "added after cache warmup".to_string(),
                    phases: Vec::new(),
                    scope: WorkflowScope::CodexHome,
                    path: LegacyAppPathString::from_path(&codex_root.join("bravo.workflow.js"),),
                },
            ],
            next_cursor: None,
        }
    );

    Ok(())
}

#[tokio::test]
async fn workflow_list_rejects_unknown_thread() -> Result<()> {
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
    let request_id = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: thread_id.to_string(),
            cursor: None,
            limit: None,
        })
        .await?;
    let JSONRPCError { error, .. } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.message, format!("thread not found: {thread_id}"));

    Ok(())
}

#[tokio::test]
async fn workflow_list_requires_workflow_feature() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    write_project_workflow_feature(
        codex_home.path(),
        workspace.path(),
        /*workflow_enabled*/ false,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp, workspace.path()).await?;

    let request_id = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: thread_id.clone(),
            cursor: None,
            limit: None,
        })
        .await?;
    let JSONRPCError { error, .. } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.message,
        format!("workflow feature is disabled for thread {thread_id}")
    );

    Ok(())
}

#[tokio::test]
async fn workflow_list_requires_startup_watcher_gate() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ false,
    )?;
    write_project_workflow_feature(
        codex_home.path(),
        workspace.path(),
        /*workflow_enabled*/ true,
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp, workspace.path()).await?;

    let request_id = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: thread_id.clone(),
            cursor: None,
            limit: None,
        })
        .await?;
    let JSONRPCError { error, .. } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(
        error.message,
        format!("workflow feature is disabled for thread {thread_id}")
    );

    Ok(())
}

#[tokio::test]
async fn workflow_list_requires_experimental_api_capability() -> Result<()> {
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
    let init = mcp
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
    let JSONRPCMessage::Response(_) = init else {
        anyhow::bail!("expected initialize response, got {init:?}");
    };

    let request_id = mcp
        .send_workflow_list_request(WorkflowListParams {
            thread_id: "00000000-0000-4000-8000-000000000001".to_string(),
            cursor: None,
            limit: None,
        })
        .await?;
    let JSONRPCError { error, .. } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    assert_eq!(error.code, -32600);
    assert_eq!(
        error.message,
        "workflow/list requires experimentalApi capability"
    );
    assert_eq!(error.data, None);

    Ok(())
}

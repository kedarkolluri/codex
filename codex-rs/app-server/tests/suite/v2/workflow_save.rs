use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::TestAppServer;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCErrorError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::WorkflowSaveDisposition;
use codex_app_server_protocol::WorkflowSaveParams;
use codex_app_server_protocol::WorkflowSaveResponse;
use codex_app_server_protocol::WorkflowSaveScope;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::skip_if_no_remote_env;
use core_test_support::skip_if_remote;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const UNAVAILABLE_MESSAGE: &str = "workflow run is unavailable for this thread";

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

fn workflow_source(name: &str, marker: &str) -> Vec<u8> {
    format!(
        "export const meta = {{ name: '{name}', description: 'save integration test' }};\nconst marker = '{marker}';\n"
    )
    .into_bytes()
}

fn write_run(
    codex_home: &Path,
    run_id: &str,
    owner_thread_id: Option<&str>,
    name: &str,
    source: &[u8],
) -> Result<()> {
    let run_dir = codex_home.join("workflows").join("runs").join(run_id);
    std::fs::create_dir_all(&run_dir)?;
    std::fs::write(run_dir.join("script.js"), source)?;
    let mut meta = json!({
        "type": "run_meta",
        "run_id": run_id,
        "parent_run_id": null,
        "script_hash": codex_workflow_journal::prompt_hash(std::str::from_utf8(source)?),
        "args_hash": "blake3:save-test-args",
        "name": name,
        "budget_total": null,
        "key_algo_version": 1,
        "created_at": "2026-07-19T00:00:00Z"
    });
    if let Some(owner_thread_id) = owner_thread_id {
        meta["owner_thread_id"] = json!(owner_thread_id);
    }
    std::fs::write(run_dir.join("meta.json"), serde_json::to_vec(&meta)?)?;
    Ok(())
}

async fn read_response(mcp: &mut TestAppServer, request_id: i64) -> Result<JSONRPCResponse> {
    timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await?
}

async fn read_error(mcp: &mut TestAppServer, request_id: i64) -> Result<JSONRPCErrorError> {
    let JSONRPCError { error, .. } = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_error_message(RequestId::Integer(request_id)),
    )
    .await??;
    Ok(error)
}

async fn start_thread(mcp: &mut TestAppServer) -> Result<String> {
    let cwd = mcp.auto_env()?.cwd().clone().into_path_buf();
    let request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let response = read_response(mcp, request_id).await?;
    Ok(
        serde_json::from_value::<ThreadStartResponse>(response.result)?
            .thread
            .id,
    )
}

async fn save(
    mcp: &mut TestAppServer,
    thread_id: &str,
    run_id: &str,
    name: &str,
    scope: WorkflowSaveScope,
    overwrite: bool,
) -> Result<i64> {
    mcp.send_workflow_save_request(WorkflowSaveParams {
        thread_id: thread_id.to_string(),
        run_id: run_id.to_string(),
        name: name.to_string(),
        scope,
        overwrite,
    })
    .await
}

async fn read_save_response(
    mcp: &mut TestAppServer,
    request_id: i64,
    disposition: WorkflowSaveDisposition,
) -> Result<()> {
    let response = read_response(mcp, request_id).await?;
    assert_eq!(
        response.result,
        serde_json::to_value(WorkflowSaveResponse { disposition })?
    );
    Ok(())
}

#[tokio::test]
async fn workflow_save_requires_experimental_api_capability() -> Result<()> {
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

    let request_id = save(
        &mut mcp,
        "00000000-0000-4000-8000-000000000001",
        "0198d9a4-9c96-7e11-8b05-f7aa47280743",
        "release-audit",
        WorkflowSaveScope::Project,
        /*overwrite*/ false,
    )
    .await?;
    assert_eq!(
        read_error(&mut mcp, request_id).await?,
        JSONRPCErrorError {
            code: -32600,
            data: None,
            message: "workflow/save requires experimentalApi capability".to_string(),
        }
    );
    Ok(())
}

#[tokio::test]
async fn workflow_save_requires_workflow_feature() -> Result<()> {
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

    let request_id = save(
        &mut mcp,
        &thread_id,
        "0198d9a4-9c96-7e11-8b05-f7aa47280743",
        "release-audit",
        WorkflowSaveScope::Project,
        /*overwrite*/ false,
    )
    .await?;
    let error = read_error(&mut mcp, request_id).await?;
    assert_eq!(
        error.message,
        format!("workflow feature is disabled for thread {thread_id}")
    );
    Ok(())
}

#[tokio::test]
async fn workflow_save_project_create_conflict_and_overwrite_are_exact() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "project workflow saves require a cwd on the app-server host"
    );
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
    let project_root = mcp.auto_env()?.cwd().clone().into_path_buf();
    let run_a = uuid::Uuid::now_v7().to_string();
    let run_b = uuid::Uuid::now_v7().to_string();
    let source_a = workflow_source("release-audit", "SOURCE_SECRET_REVISION_A");
    let source_b = workflow_source("release-audit", "SOURCE_SECRET_REVISION_B");
    write_run(
        codex_home.path(),
        &run_a,
        Some(&thread_id),
        "release-audit",
        &source_a,
    )?;
    write_run(
        codex_home.path(),
        &run_b,
        Some(&thread_id),
        "release-audit",
        &source_b,
    )?;

    let created = save(
        &mut mcp,
        &thread_id,
        &run_a,
        "release-audit",
        WorkflowSaveScope::Project,
        /*overwrite*/ false,
    )
    .await?;
    read_save_response(&mut mcp, created, WorkflowSaveDisposition::Created).await?;
    let target = project_root
        .join(".codex")
        .join("workflows")
        .join("release-audit.js");
    assert_eq!(std::fs::read(&target)?, source_a);

    let conflict = save(
        &mut mcp,
        &thread_id,
        &run_b,
        "release-audit",
        WorkflowSaveScope::Project,
        /*overwrite*/ false,
    )
    .await?;
    read_save_response(&mut mcp, conflict, WorkflowSaveDisposition::Conflict).await?;
    assert_eq!(std::fs::read(&target)?, source_a);

    let overwritten = save(
        &mut mcp,
        &thread_id,
        &run_b,
        "release-audit",
        WorkflowSaveScope::Project,
        /*overwrite*/ true,
    )
    .await?;
    read_save_response(&mut mcp, overwritten, WorkflowSaveDisposition::Overwritten).await?;
    assert_eq!(std::fs::read(&target)?, source_b);
    assert!(!project_root.join(".claude").exists());
    Ok(())
}

#[tokio::test]
async fn workflow_save_project_rejects_a_remote_thread_cwd() -> Result<()> {
    skip_if_no_remote_env!(Ok(()));
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
    let run_id = uuid::Uuid::now_v7().to_string();
    let source = workflow_source("remote-audit", "REMOTE_SOURCE_SECRET");
    write_run(
        codex_home.path(),
        &run_id,
        Some(&thread_id),
        "remote-audit",
        &source,
    )?;

    let request_id = save(
        &mut mcp,
        &thread_id,
        &run_id,
        "remote-audit",
        WorkflowSaveScope::Project,
        /*overwrite*/ false,
    )
    .await?;
    assert_eq!(
        read_error(&mut mcp, request_id).await?,
        JSONRPCErrorError {
            code: -32600,
            data: None,
            message: "workflow save destination is unavailable".to_string(),
        }
    );
    Ok(())
}

#[tokio::test]
async fn workflow_save_personal_uses_agents_root_and_never_logs_source() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    let personal_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let personal_home_string = personal_home.path().to_string_lossy().into_owned();
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[
            ("HOME", Some(&personal_home_string)),
            ("USERPROFILE", Some(&personal_home_string)),
        ])
        .with_json_logging("info")
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;
    let run_id = uuid::Uuid::now_v7().to_string();
    let source = workflow_source("personal-audit", "PERSONAL_SOURCE_SECRET");
    write_run(
        codex_home.path(),
        &run_id,
        Some(&thread_id),
        "personal-audit",
        &source,
    )?;

    let request_id = save(
        &mut mcp,
        &thread_id,
        &run_id,
        "personal-audit",
        WorkflowSaveScope::Personal,
        /*overwrite*/ false,
    )
    .await?;
    read_save_response(&mut mcp, request_id, WorkflowSaveDisposition::Created).await?;
    let target = personal_home
        .path()
        .join(".agents")
        .join("workflows")
        .join("personal-audit.js");
    assert_eq!(std::fs::read(target)?, source);
    assert!(!personal_home.path().join(".claude").exists());

    let logs = serde_json::to_string(&mcp.json_log_events()?)?;
    assert!(!logs.contains("PERSONAL_SOURCE_SECRET"));
    assert!(!logs.contains("blake3:save-test-args"));
    Ok(())
}

#[tokio::test]
async fn workflow_save_run_authorization_is_non_leaking() -> Result<()> {
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    let personal_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let personal_home_string = personal_home.path().to_string_lossy().into_owned();
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[
            ("HOME", Some(&personal_home_string)),
            ("USERPROFILE", Some(&personal_home_string)),
        ])
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let owner_thread_id = start_thread(&mut mcp).await?;
    let other_thread_id = start_thread(&mut mcp).await?;
    let owned_run_id = uuid::Uuid::now_v7().to_string();
    let ownerless_run_id = uuid::Uuid::now_v7().to_string();
    let metadata_mismatch_run_id = uuid::Uuid::now_v7().to_string();
    let unknown_run_id = uuid::Uuid::now_v7().to_string();
    let source = workflow_source("private-audit", "AUTHORIZATION_SECRET");
    write_run(
        codex_home.path(),
        &owned_run_id,
        Some(&owner_thread_id),
        "private-audit",
        &source,
    )?;
    write_run(
        codex_home.path(),
        &ownerless_run_id,
        /*owner_thread_id*/ None,
        "private-audit",
        &source,
    )?;
    write_run(
        codex_home.path(),
        &metadata_mismatch_run_id,
        Some(&owner_thread_id),
        "different-name",
        &source,
    )?;

    let noncanonical_run_id = format!("{{{owned_run_id}}}");
    let cases = [
        (&other_thread_id, owned_run_id.as_str()),
        (&owner_thread_id, unknown_run_id.as_str()),
        (&owner_thread_id, ownerless_run_id.as_str()),
        (&owner_thread_id, metadata_mismatch_run_id.as_str()),
        (&owner_thread_id, "not-a-uuid"),
        (&owner_thread_id, noncanonical_run_id.as_str()),
    ];
    let mut errors = Vec::new();
    for (thread_id, run_id) in cases {
        let request_id = save(
            &mut mcp,
            thread_id,
            run_id,
            "private-audit",
            WorkflowSaveScope::Personal,
            /*overwrite*/ false,
        )
        .await?;
        errors.push(read_error(&mut mcp, request_id).await?);
    }

    let expected = JSONRPCErrorError {
        code: -32600,
        data: None,
        message: UNAVAILABLE_MESSAGE.to_string(),
    };
    assert_eq!(errors, vec![expected; 6]);
    assert!(UNAVAILABLE_MESSAGE.len() < 64);
    Ok(())
}

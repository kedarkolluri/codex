use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::DEFAULT_CLIENT_NAME;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::ClientInfo;
use codex_app_server_protocol::InitializeCapabilities;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::WorkflowsChangedNotification;
use codex_features::Feature;
use core_test_support::responses;
use core_test_support::skip_if_remote;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const WATCHER_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait while asserting that *no* `workflows/changed` arrives. Must
/// comfortably exceed the (test-mode) watcher throttle interval so a watcher, if
/// one existed, would have fired.
const ABSENCE_WINDOW: Duration = Duration::from_secs(3);

fn workflow_source(name: &str, description: &str) -> String {
    format!("export const meta = {{ name: '{name}', description: '{description}' }};\n")
}

/// Write a `config.toml` for a mock provider, optionally enabling the
/// experimental `workflow` feature.
fn write_config(
    codex_home: &std::path::Path,
    server_uri: &str,
    workflow_enabled: bool,
) -> Result<()> {
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

/// Acceptance: with `Feature::Workflow` enabled, adding a `*.workflow.js` file
/// under a watched static root (`$CODEX_HOME/workflows`) emits a single
/// `workflows/changed` notification (mirrors the skills watcher).
#[tokio::test]
async fn workflows_changed_notification_is_emitted_after_workflow_change() -> Result<()> {
    // The watcher observes host-local filesystem changes; remote executors do
    // not surface those, matching the skills watcher's remote skip.
    skip_if_remote!(
        Ok(()),
        "host-local workflow changes are not visible to remote executors"
    );

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;

    // Pre-create the CODEX_HOME workflows root so the watcher registers a direct
    // recursive watch on it, making file creation events deterministic.
    let workflows_dir = codex_home.path().join("workflows");
    std::fs::create_dir_all(&workflows_dir)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    // Create a new saved workflow under the watched root.
    std::fs::write(
        workflows_dir.join("demo.workflow.js"),
        workflow_source("demo", "demo description"),
    )?;

    let notification = timeout(
        WATCHER_TIMEOUT,
        mcp.read_stream_until_notification_message("workflows/changed"),
    )
    .await??;
    let params = notification
        .params
        .context("workflows/changed params must be present")?;
    let notification: WorkflowsChangedNotification = serde_json::from_value(params)?;
    assert_eq!(notification, WorkflowsChangedNotification {});

    Ok(())
}

/// Backward compatibility: the saved-workflow invalidation notification was
/// stable before workflow controls were introduced, so it must still reach a
/// connection that did not opt into the experimental app-server API.
#[tokio::test]
async fn workflows_changed_notification_remains_stable_without_experimental_api() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "host-local workflow changes are not visible to remote executors"
    );

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let workflows_dir = codex_home.path().join("workflows");
    std::fs::create_dir_all(&workflows_dir)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    mcp.initialize_with_capabilities(
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

    std::fs::write(
        workflows_dir.join("demo.workflow.js"),
        workflow_source("demo", "demo description"),
    )?;

    let notification = timeout(
        WATCHER_TIMEOUT,
        mcp.read_stream_until_notification_message("workflows/changed"),
    )
    .await??;
    let params = notification
        .params
        .context("workflows/changed params must be present")?;
    let notification: WorkflowsChangedNotification = serde_json::from_value(params)?;
    assert_eq!(notification, WorkflowsChangedNotification {});

    Ok(())
}

/// Gating: with `Feature::Workflow` disabled, no watcher is constructed, so a
/// workflow file change under a would-be-watched root emits nothing.
#[tokio::test]
async fn no_workflows_changed_notification_when_feature_disabled() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "host-local workflow changes are not visible to remote executors"
    );

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ false,
    )?;

    // Same root the enabled case watches — the only difference is the feature
    // gate, so any emission here would be the watcher running while disabled.
    let workflows_dir = codex_home.path().join("workflows");
    std::fs::create_dir_all(&workflows_dir)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    std::fs::write(
        workflows_dir.join("demo.workflow.js"),
        workflow_source("demo", "demo description"),
    )?;

    let result = timeout(
        ABSENCE_WINDOW,
        mcp.read_stream_until_notification_message("workflows/changed"),
    )
    .await;
    assert!(
        result.is_err(),
        "workflows/changed must not be emitted while Feature::Workflow is disabled"
    );

    Ok(())
}

/// Durable run artifacts share the Codex-home `workflows` parent directory with
/// saved definitions, but changes below its reserved top-level `runs` subtree
/// must not invalidate the saved-workflow registry or notify clients.
#[tokio::test]
async fn no_workflows_changed_notification_for_codex_home_run_artifacts() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "host-local workflow changes are not visible to remote executors"
    );

    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    let run_dir = codex_home
        .path()
        .join("workflows")
        .join("runs")
        .join("run-id");
    std::fs::create_dir_all(&run_dir)?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;

    std::fs::write(
        run_dir.join("script.js"),
        workflow_source("historical-run", "durable execution state"),
    )?;

    let result = timeout(
        ABSENCE_WINDOW,
        mcp.read_stream_until_notification_message("workflows/changed"),
    )
    .await;
    assert!(
        result.is_err(),
        "workflows/changed must not be emitted for durable run artifacts"
    );
    Ok(())
}

/// Per-thread roots: a thread attaching in a directory contributes
/// `<that cwd>/.codex/workflows` to the watched set (mirrors how the skills
/// watcher derives roots per thread at listener-attach time). A change there
/// then emits `workflows/changed`.
#[tokio::test]
async fn workflows_changed_notification_is_emitted_for_thread_project_root() -> Result<()> {
    skip_if_remote!(
        Ok(()),
        "host-local workflow changes are not visible to remote executors"
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

    // The thread's primary selected environment cwd is the project root the
    // listener registers on attach (`<cwd>/.codex/workflows`). Pin config cwd to
    // that same existing host-local temp dir and pre-create the workflows dir so
    // the watcher registers a direct recursive watch on it.
    let thread_cwd = mcp.auto_env()?.cwd().clone().into_path_buf();
    let project_workflows_dir = thread_cwd.join(".codex").join("workflows");
    std::fs::create_dir_all(&project_workflows_dir)?;

    // Start a thread on the auto environment so its listener attaches and
    // registers the project root. `cwd` must be set explicitly: when omitted it
    // defaults to CODEX_HOME rather than the environment cwd.
    let request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            cwd: Some(thread_cwd.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let ThreadStartResponse { .. } = to_response::<ThreadStartResponse>(response)?;

    // A change under the thread's project root must be observed even though it is
    // not a static (config-level) root.
    std::fs::write(
        project_workflows_dir.join("project.workflow.js"),
        workflow_source("project", "project scoped workflow"),
    )?;

    let notification = timeout(
        WATCHER_TIMEOUT,
        mcp.read_stream_until_notification_message("workflows/changed"),
    )
    .await??;
    let params = notification
        .params
        .context("workflows/changed params must be present")?;
    let notification: WorkflowsChangedNotification = serde_json::from_value(params)?;
    assert_eq!(notification, WorkflowsChangedNotification {});

    Ok(())
}

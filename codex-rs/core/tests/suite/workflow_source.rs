use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_core_workflows::WORKFLOW_SOURCE_MAX_BYTES;
use codex_core_workflows::WorkflowRoot;
use codex_core_workflows::load_workflows_from_roots;
use codex_exec_server::CreateDirectoryOptions;
use core_test_support::is_remote_test_environment;
use core_test_support::test_codex::TestEnv;
use core_test_support::test_codex::test_env;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::ChildStdout;
use tokio::process::Command;
use tokio::time::Instant;
use tokio::time::timeout;

const EXEC_SERVER_START_TIMEOUT: Duration = Duration::from_secs(30);
const VERIFIED_CAPTURE_REPETITIONS: usize = 129;

struct LocalExecServer {
    _codex_home: TempDir,
    child: Child,
    _stdout: BufReader<ChildStdout>,
    websocket_url: String,
}

impl LocalExecServer {
    async fn start() -> Result<Self> {
        let codex_home = TempDir::new()?;
        let mut child = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
            .args(["exec-server", "--listen", "ws://127.0.0.1:0"])
            .env("CODEX_HOME", codex_home.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let stdout = child
            .stdout
            .take()
            .context("exec-server stdout should be piped")?;
        let mut stdout = BufReader::new(stdout);
        let deadline = Instant::now() + EXEC_SERVER_START_TIMEOUT;
        let websocket_url = loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .context("timed out waiting for exec-server listen URL")?;
            let mut line = String::new();
            let bytes_read = timeout(remaining, stdout.read_line(&mut line))
                .await
                .context("timed out reading exec-server listen URL")??;
            if bytes_read == 0 {
                bail!("exec-server exited before printing its listen URL");
            }
            let line = line.trim();
            if line.starts_with("ws://") {
                break line.to_string();
            }
        };

        Ok(Self {
            _codex_home: codex_home,
            child,
            _stdout: stdout,
            websocket_url,
        })
    }
}

impl Drop for LocalExecServer {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn workflow_source(marker: &str, size: usize) -> String {
    let mut source = format!(
        "export const meta = {{ name: 'review', description: 'Review changes', phases: ['Inspect', 'Report'] }};\n// {marker}\n"
    );
    assert!(
        source.len() <= size,
        "workflow fixture exceeds requested size"
    );
    source.push_str(&" ".repeat(size - source.len()));
    source
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_registry_captures_verified_source_over_selected_websocket_executor() -> Result<()>
{
    let (test_env, _local_exec_server) = if is_remote_test_environment() {
        (test_env().await?, None)
    } else {
        let exec_server = LocalExecServer::start().await?;
        let environment =
            TestEnv::local_with_exec_server_url(Some(exec_server.websocket_url.clone())).await?;
        (environment, Some(exec_server))
    };
    let file_system = test_env.environment().get_filesystem();
    let workflow_root = test_env.selection().cwd.join(".codex/workflows")?;
    let workflow_path = workflow_root.join("review.workflow.js")?;
    file_system
        .create_directory(
            &workflow_root,
            CreateDirectoryOptions { recursive: true },
            /*sandbox*/ None,
        )
        .await?;

    let initial_source = workflow_source("initial-retained-source", WORKFLOW_SOURCE_MAX_BYTES);
    file_system
        .write_file(
            &workflow_path,
            initial_source.clone().into_bytes(),
            /*sandbox*/ None,
        )
        .await?;
    let registry = load_workflows_from_roots([WorkflowRoot::project_on_executor(
        workflow_root.clone(),
        file_system.clone(),
    )])
    .await;
    assert_eq!(registry.errors(), &[]);
    assert_eq!(registry.names().collect::<Vec<_>>(), vec!["review"]);
    let metadata = registry
        .resolve_by_name("review")
        .context("workflow should be discovered over the selected executor")?
        .clone();

    let initial_snapshot = registry
        .source_snapshot_by_name("review")
        .await?
        .context("workflow source should be captured")?;
    assert_eq!(initial_snapshot.metadata(), &metadata);
    assert_eq!(initial_snapshot.source(), initial_source);

    file_system
        .write_file(
            &workflow_path,
            format!("{initial_source}x").into_bytes(),
            /*sandbox*/ None,
        )
        .await?;
    let error = registry
        .source_snapshot_by_name("review")
        .await
        .expect_err("the verified capture must reject a source larger than 1 MiB");
    assert_eq!(
        (error.path(), error.message()),
        (&metadata.path, "failed to capture source (InvalidInput)"),
    );
    assert_eq!(initial_snapshot.source(), initial_source);

    let replacement_source = workflow_source("replacement-retained-source", /*size*/ 256);
    file_system
        .write_file(
            &workflow_path,
            replacement_source.clone().into_bytes(),
            /*sandbox*/ None,
        )
        .await?;
    // More than 128 sequential captures prove the real transport closes each stream and releases
    // its handle slot before the next invocation.
    for _ in 0..VERIFIED_CAPTURE_REPETITIONS {
        let replacement_snapshot = registry
            .source_snapshot_by_name("review")
            .await?
            .context("a later invocation should capture the replacement source")?;
        assert_eq!(replacement_snapshot.metadata(), &metadata);
        assert_eq!(replacement_snapshot.source(), replacement_source);
    }
    assert_eq!(initial_snapshot.source(), initial_source);

    Ok(())
}

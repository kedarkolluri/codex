use std::fs;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::ensure;
use codex_code_mode_protocol::host::Capability;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
use codex_code_mode_protocol::host::WireExecuteRequest;
use codex_code_mode_protocol::host::WireResult;
use codex_code_mode_protocol::host::WireWorkflowCellId;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::process::Command;

use super::host;

pub(super) const MISMATCHED_WORKFLOW_ID_MODE: &str = "mismatched-workflow-id";
pub(super) const STDIN_OBSERVER_ARG: &str = "--stdin-observer";
pub(super) const STDOUT_OBSERVER_ARG: &str = "--stdout-observer";

const POST_MISMATCH_MARKER_SUFFIX: &str = ".post-mismatch-client-message";
const MISMATCHED_RESPONSE_SENT_SUFFIX: &str = ".mismatched-response-sent";
const POST_MISMATCH_OBSERVER_ARMED_SUFFIX: &str = ".post-mismatch-observer-armed";
const POST_MISMATCH_OBSERVER_DONE_SUFFIX: &str = ".post-mismatch-observer-done";
const PREVIOUS_OBSERVER_TIMEOUT: Duration = Duration::from_secs(5);
const PREVIOUS_OBSERVER_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub(super) enum FixtureRole {
    Host,
    StdinObserver,
    StdoutObserver,
}

pub(super) async fn run(current_exe: PathBuf, role: FixtureRole) -> Result<()> {
    let post_mismatch_marker = sidecar_path(&current_exe, POST_MISMATCH_MARKER_SUFFIX);
    let mismatched_response_sent = sidecar_path(&current_exe, MISMATCHED_RESPONSE_SENT_SUFFIX);
    let observer_armed = sidecar_path(&current_exe, POST_MISMATCH_OBSERVER_ARMED_SUFFIX);
    let observer_done = sidecar_path(&current_exe, POST_MISMATCH_OBSERVER_DONE_SUFFIX);
    match role {
        FixtureRole::Host => {}
        FixtureRole::StdinObserver => {
            return run_stdin_observer(&post_mismatch_marker, &observer_armed, &observer_done)
                .await;
        }
        FixtureRole::StdoutObserver => {
            return run_stdout_observer(&mismatched_response_sent).await;
        }
    }

    prepare_observer_sidecars(&observer_armed, &observer_done).await?;
    let mut stdin_observer = Command::new(&current_exe)
        .arg(STDIN_OBSERVER_ARG)
        .stdin(Stdio::inherit())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn fixture stdin observer")?;
    let stdin_observer_stdout = stdin_observer
        .stdout
        .take()
        .context("fixture stdin observer has no stdout")?;
    let mut stdout_observer = Command::new(&current_exe)
        .arg(STDOUT_OBSERVER_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to spawn fixture stdout observer")?;
    let stdout_observer_stdin = stdout_observer
        .stdin
        .take()
        .context("fixture stdout observer has no stdin")?;
    let selected_capabilities = CapabilitySet::try_new([
        Capability::new(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY)?,
        Capability::new(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY)?,
    ])?;
    host::run_fixture(
        FramedReader::new(stdin_observer_stdout),
        FramedWriter::new(stdout_observer_stdin),
        selected_capabilities,
        MismatchedIdentity { observer_armed },
    )
    .await
}

struct MismatchedIdentity {
    observer_armed: PathBuf,
}

impl host::SavedExecuteHandler for MismatchedIdentity {
    fn handle<'a, R, W>(
        &'a self,
        id: RequestId,
        request: WireExecuteRequest,
        reader: &'a mut FramedReader<R>,
        writer: &'a mut FramedWriter<W>,
    ) -> impl Future<Output = Result<()>> + Send + 'a
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        let observer_armed = self.observer_armed.clone();
        async move {
            let workflow_cell_id = request.workflow_cell_id.context(
                "paired-capability fixture received Saved execute without a workflow cell identity",
            )?;
            let mismatched_sequence = workflow_cell_id
                .sequence()
                .checked_add(1)
                .context("fixture cannot increment workflow cell sequence")?;
            let mismatched_cell_id = WireWorkflowCellId::try_new(format!(
                "wf:1:{}:{mismatched_sequence}",
                workflow_cell_id.epoch()
            ))?;
            write_observer_sidecar(&observer_armed, b"armed\n")?;
            writer
                .write(&HostToClient::Response {
                    id,
                    result: WireResult::Ok {
                        value: HostResponse::ExecutionStarted {
                            cell_id: mismatched_cell_id.into(),
                        },
                    },
                })
                .await
                .context("failed to write mismatched execution-started response")?;
            ensure!(
                reader
                    .read::<ClientToHost>()
                    .await
                    .context("failed to await fixture stdin observer EOF")?
                    .is_none(),
                "fixture stdin observer forwarded a post-mismatch message"
            );
            Ok(())
        }
    }
}

fn sidecar_path(current_exe: &Path, suffix: &str) -> PathBuf {
    let mut sidecar_name = current_exe.as_os_str().to_os_string();
    sidecar_name.push(suffix);
    PathBuf::from(sidecar_name)
}

fn observer_sidecar_exists(path: &Path) -> Result<bool> {
    path.try_exists().with_context(|| {
        format!(
            "failed to inspect fixture observer sidecar `{}`",
            path.display()
        )
    })
}

fn write_observer_sidecar(path: &Path, contents: &[u8]) -> Result<()> {
    fs::write(path, contents).with_context(|| {
        format!(
            "failed to write fixture observer sidecar `{}`",
            path.display()
        )
    })
}

fn remove_observer_sidecar(path: &Path) -> Result<()> {
    fs::remove_file(path).with_context(|| {
        format!(
            "failed to remove stale fixture observer sidecar `{}`",
            path.display()
        )
    })
}

async fn prepare_observer_sidecars(observer_armed: &Path, observer_done: &Path) -> Result<()> {
    let armed_exists = observer_sidecar_exists(observer_armed)?;
    let mut done_exists = observer_sidecar_exists(observer_done)?;
    if armed_exists && !done_exists {
        let deadline = Instant::now() + PREVIOUS_OBSERVER_TIMEOUT;
        while !done_exists {
            ensure!(
                Instant::now() < deadline,
                "timed out waiting for previous fixture stdin observer to drain"
            );
            tokio::time::sleep(PREVIOUS_OBSERVER_POLL_INTERVAL).await;
            done_exists = observer_sidecar_exists(observer_done)?;
        }
    }
    if observer_sidecar_exists(observer_armed)? {
        remove_observer_sidecar(observer_armed)?;
    }
    if done_exists {
        remove_observer_sidecar(observer_done)?;
    }
    Ok(())
}

async fn run_stdin_observer(
    post_mismatch_marker: &Path,
    observer_armed: &Path,
    observer_done: &Path,
) -> Result<()> {
    let mut reader = FramedReader::new(tokio::io::stdin());
    let mut writer = FramedWriter::new(tokio::io::stdout());
    while let Some(message) = reader
        .read::<ClientToHost>()
        .await
        .context("fixture stdin observer failed to read client message")?
    {
        if observer_sidecar_exists(observer_armed)? {
            let mut marker = OpenOptions::new()
                .create(true)
                .append(true)
                .open(post_mismatch_marker)
                .with_context(|| {
                    format!(
                        "failed to open post-mismatch marker `{}`",
                        post_mismatch_marker.display()
                    )
                })?;
            writeln!(marker, "{}", client_message_kind(&message)).with_context(|| {
                format!(
                    "failed to write post-mismatch marker `{}`",
                    post_mismatch_marker.display()
                )
            })?;
        } else {
            writer
                .write(&message)
                .await
                .context("fixture stdin observer failed to forward client message")?;
        }
    }
    write_observer_sidecar(observer_done, b"done\n")?;
    Ok(())
}

async fn run_stdout_observer(mismatched_response_sent: &Path) -> Result<()> {
    let mut reader = FramedReader::new(tokio::io::stdin());
    let mut writer = FramedWriter::new(tokio::io::stdout());
    while let Some(message) = reader
        .read::<HostToClient>()
        .await
        .context("fixture stdout observer failed to read host message")?
    {
        let execution_started = matches!(
            &message,
            HostToClient::Response {
                result: WireResult::Ok {
                    value: HostResponse::ExecutionStarted { .. },
                },
                ..
            }
        );
        writer
            .write(&message)
            .await
            .context("fixture stdout observer failed to forward host message")?;
        if execution_started {
            write_observer_sidecar(mismatched_response_sent, b"sent\n")?;
        }
    }
    Ok(())
}

fn client_message_kind(message: &ClientToHost) -> &'static str {
    match message {
        ClientToHost::ClientHello(_) => "connection/hello",
        ClientToHost::CancelRequest { .. } => "operation/cancel",
        ClientToHost::DelegateResponse { .. } => "delegate/response",
        ClientToHost::Request { request, .. } => match request {
            HostRequest::OpenSession { .. } => "operation/request:session/open",
            HostRequest::Execute { .. } => "operation/request:session/execute",
            HostRequest::Wait { .. } => "operation/request:session/wait",
            HostRequest::Terminate { .. } => "operation/request:session/terminate",
            HostRequest::ShutdownSession { .. } => "operation/request:session/shutdown",
        },
    }
}

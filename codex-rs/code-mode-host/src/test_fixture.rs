use std::ffi::OsStr;
use std::fs;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use anyhow::ensure;
use codex_code_mode_protocol::host::Capability;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HostHello;
use codex_code_mode_protocol::host::HostRequest;
use codex_code_mode_protocol::host::HostResponse;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireContentItem;
use codex_code_mode_protocol::host::WireExecuteOutputPolicy;
use codex_code_mode_protocol::host::WireResult;
use codex_code_mode_protocol::host::WireRuntimeResponse;
use codex_code_mode_protocol::host::WireWorkflowCellId;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::process::Command;

const NO_SAVED_CAPABILITIES_MODE: &str = "no-saved-capabilities";
const LEGACY_OUTPUT_ONLY_MODE: &str = "legacy-output-only";
const INVALID_WORKFLOW_CAPABILITIES_MODE: &str = "invalid-workflow-capabilities";
const MISMATCHED_WORKFLOW_ID_MODE: &str = "mismatched-workflow-id";
const STDIN_OBSERVER_ARG: &str = "--stdin-observer";
// The integration test appends these literals to the full copied executable path.
const POST_MISMATCH_MARKER_SUFFIX: &str = ".post-mismatch-client-message";
const POST_MISMATCH_OBSERVER_ARMED_SUFFIX: &str = ".post-mismatch-observer-armed";
const POST_MISMATCH_OBSERVER_DONE_SUFFIX: &str = ".post-mismatch-observer-done";
const ORDINARY_CELL_ID: &str = "1";
const PREVIOUS_OBSERVER_TIMEOUT: Duration = Duration::from_secs(5);
const PREVIOUS_OBSERVER_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FixtureMode {
    NoSavedCapabilities,
    LegacyOutputOnly,
    InvalidWorkflowCapabilities,
    MismatchedWorkflowId,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to resolve fixture executable")?;
    let executable_stem = current_exe
        .file_stem()
        .context("fixture executable has no file stem")?;
    let mode = if executable_stem == OsStr::new(NO_SAVED_CAPABILITIES_MODE) {
        FixtureMode::NoSavedCapabilities
    } else if executable_stem == OsStr::new(LEGACY_OUTPUT_ONLY_MODE) {
        FixtureMode::LegacyOutputOnly
    } else if executable_stem == OsStr::new(INVALID_WORKFLOW_CAPABILITIES_MODE) {
        FixtureMode::InvalidWorkflowCapabilities
    } else if executable_stem == OsStr::new(MISMATCHED_WORKFLOW_ID_MODE) {
        FixtureMode::MismatchedWorkflowId
    } else {
        bail!(
            "unknown fixture executable stem `{}`; expected `{NO_SAVED_CAPABILITIES_MODE}`, \
             `{LEGACY_OUTPUT_ONLY_MODE}`, `{INVALID_WORKFLOW_CAPABILITIES_MODE}`, or \
             `{MISMATCHED_WORKFLOW_ID_MODE}`",
            executable_stem.to_string_lossy()
        );
    };
    let mut args = std::env::args_os().skip(1);
    let stdin_observer = match args.next() {
        None => false,
        Some(arg) if arg == OsStr::new(STDIN_OBSERVER_ARG) => true,
        Some(arg) => bail!("unknown fixture argument `{}`", arg.to_string_lossy()),
    };
    ensure!(args.next().is_none(), "fixture received too many arguments");

    let post_mismatch_marker = sidecar_path(&current_exe, POST_MISMATCH_MARKER_SUFFIX);
    let observer_armed = sidecar_path(&current_exe, POST_MISMATCH_OBSERVER_ARMED_SUFFIX);
    let observer_done = sidecar_path(&current_exe, POST_MISMATCH_OBSERVER_DONE_SUFFIX);
    if stdin_observer {
        ensure!(
            mode == FixtureMode::MismatchedWorkflowId,
            "stdin observer is only valid for the mismatched workflow-ID fixture"
        );
        return run_stdin_observer(
            &post_mismatch_marker,
            &observer_armed,
            &observer_done,
        )
        .await;
    }

    match mode {
        FixtureMode::NoSavedCapabilities
        | FixtureMode::LegacyOutputOnly
        | FixtureMode::InvalidWorkflowCapabilities => {
            run_fixture(
                mode,
                FramedReader::new(tokio::io::stdin()),
                FramedWriter::new(tokio::io::stdout()),
                &observer_armed,
            )
            .await
        }
        FixtureMode::MismatchedWorkflowId => {
            prepare_observer_sidecars(&observer_armed, &observer_done).await?;
            let mut observer = Command::new(&current_exe)
                .arg(STDIN_OBSERVER_ARG)
                .stdin(Stdio::inherit())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .context("failed to spawn fixture stdin observer")?;
            let observer_stdout = observer
                .stdout
                .take()
                .context("fixture stdin observer has no stdout")?;
            run_fixture(
                mode,
                FramedReader::new(observer_stdout),
                FramedWriter::new(tokio::io::stdout()),
                &observer_armed,
            )
            .await
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

async fn run_fixture<R, W>(
    mode: FixtureMode,
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    observer_armed: &Path,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{

    let first_message = reader
        .read::<ClientToHost>()
        .await
        .context("failed to read fixture client hello")?
        .context("client closed before sending a fixture client hello")?;
    let client_hello = match first_message {
        ClientToHost::ClientHello(client_hello) => client_hello,
        ClientToHost::Request { .. } => {
            bail!("first fixture client message was operation/request, not connection/hello")
        }
        ClientToHost::CancelRequest { .. } => {
            bail!("first fixture client message was operation/cancel, not connection/hello")
        }
        ClientToHost::DelegateResponse { .. } => {
            bail!("first fixture client message was delegate/response, not connection/hello")
        }
    };
    ensure!(
        client_hello
            .supported_versions()
            .contains(ProtocolVersion::V2),
        "fixture client does not support protocol V2"
    );

    let selected_capabilities = match mode {
        FixtureMode::NoSavedCapabilities => CapabilitySet::empty(),
        FixtureMode::LegacyOutputOnly => CapabilitySet::try_new([Capability::new(
            SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY,
        )?])?,
        FixtureMode::InvalidWorkflowCapabilities => CapabilitySet::try_new([Capability::new(
            SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY,
        )?])?,
        FixtureMode::MismatchedWorkflowId => CapabilitySet::try_new([
            Capability::new(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY)?,
            Capability::new(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY)?,
        ])?,
    };
    for capability in selected_capabilities.iter() {
        ensure!(
            client_hello.required_capabilities().contains(capability)
                || client_hello.optional_capabilities().contains(capability),
            "fixture client did not offer selected capability `{capability}`"
        );
    }
    for capability in client_hello.required_capabilities().iter() {
        ensure!(
            selected_capabilities.contains(capability),
            "fixture mode does not support required capability `{capability}`"
        );
    }
    writer
        .write(&HostToClient::HostHello(HostHello::new(
            ProtocolVersion::V2,
            selected_capabilities,
        )))
        .await
        .context("failed to write fixture host hello")?;
    if mode == FixtureMode::InvalidWorkflowCapabilities {
        ensure!(
            reader
                .read::<ClientToHost>()
                .await
                .context("failed to await invalid-capability client disconnect")?
                .is_none(),
            "invalid-capability fixture received an operation after its host hello"
        );
        return Ok(());
    }

    let mut opened_session: Option<SessionId> = None;
    loop {
        let message = reader
            .read::<ClientToHost>()
            .await
            .context("failed to read fixture client message")?
            .context("fixture client closed before shutting down its session")?;
        match message {
            ClientToHost::ClientHello(_) => {
                bail!("fixture received a second client hello")
            }
            ClientToHost::CancelRequest { .. } => {
                bail!("fixture received an unexpected request cancellation")
            }
            ClientToHost::DelegateResponse { .. } => {
                bail!("fixture received an unexpected delegate response")
            }
            ClientToHost::Request { id, request } => match request {
                HostRequest::OpenSession { session_id } => {
                    ensure!(
                        opened_session.is_none(),
                        "fixture supports only one opened session"
                    );
                    opened_session = Some(session_id.clone());
                    writer
                        .write(&HostToClient::Response {
                            id,
                            result: WireResult::Ok {
                                value: HostResponse::SessionReady { session_id },
                            },
                        })
                        .await
                        .context("failed to write fixture session-ready response")?;
                }
                HostRequest::Execute {
                    session_id,
                    request,
                } => {
                    ensure!(
                        opened_session.as_ref() == Some(&session_id),
                        "fixture received execute for unopened session `{session_id}`"
                    );
                    match request.output_policy {
                        WireExecuteOutputPolicy::Ordinary => {
                            ensure!(
                                request.workflow_cell_id.is_none(),
                                "ordinary fixture execute carried a workflow cell identity"
                            );
                            let cell_id = WireCellId::try_new(ORDINARY_CELL_ID)?;
                            writer
                                .write(&HostToClient::Response {
                                    id,
                                    result: WireResult::Ok {
                                        value: HostResponse::ExecutionStarted {
                                            cell_id: cell_id.clone(),
                                        },
                                    },
                                })
                                .await
                                .context("failed to write fixture execution-started response")?;
                            writer
                                .write(&HostToClient::InitialResponse {
                                    id,
                                    result: WireResult::Ok {
                                        value: WireRuntimeResponse::Result {
                                            cell_id: cell_id.clone(),
                                            content_items: vec![WireContentItem::InputText {
                                                text: "fixture ordinary".to_string(),
                                            }],
                                            error_text: None,
                                        },
                                    },
                                })
                                .await
                                .context("failed to write fixture initial response")?;
                            writer
                                .write(&HostToClient::CellClosed {
                                    session_id,
                                    cell_id,
                                })
                                .await
                                .context("failed to write fixture cell-closed event")?;
                        }
                        WireExecuteOutputPolicy::SavedWorkflow => match mode {
                            FixtureMode::NoSavedCapabilities => {
                                bail!("no-capability fixture received a Saved execute")
                            }
                            FixtureMode::LegacyOutputOnly => {
                                bail!("legacy output-only fixture received a Saved execute")
                            }
                            FixtureMode::InvalidWorkflowCapabilities => {
                                bail!("invalid-capability fixture entered its operation loop")
                            }
                            FixtureMode::MismatchedWorkflowId => {
                                let workflow_cell_id = request.workflow_cell_id.context(
                                    "paired-capability fixture received Saved execute without a \
                                     workflow cell identity",
                                )?;
                                let mismatched_sequence = workflow_cell_id
                                    .sequence()
                                    .checked_add(1)
                                    .context("fixture cannot increment workflow cell sequence")?;
                                let mismatched_cell_id = WireWorkflowCellId::try_new(format!(
                                    "wf:1:{}:{mismatched_sequence}",
                                    workflow_cell_id.epoch()
                                ))?;
                                write_observer_sidecar(observer_armed, b"armed\n")?;
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
                                    .context(
                                        "failed to write mismatched execution-started response",
                                    )?;
                                ensure!(
                                    reader
                                        .read::<ClientToHost>()
                                        .await
                                        .context("failed to await fixture stdin observer EOF")?
                                        .is_none(),
                                    "fixture stdin observer forwarded a post-mismatch message"
                                );
                                return Ok(());
                            }
                        },
                    }
                }
                HostRequest::Wait { .. } => {
                    bail!("fixture received an unexpected wait request")
                }
                HostRequest::Terminate { .. } => {
                    bail!("fixture received an unexpected terminate request")
                }
                HostRequest::ShutdownSession { session_id } => {
                    ensure!(
                        opened_session.as_ref() == Some(&session_id),
                        "fixture received shutdown for unopened session `{session_id}`"
                    );
                    writer
                        .write(&HostToClient::Response {
                            id,
                            result: WireResult::Ok {
                                value: HostResponse::SessionClosed { session_id },
                            },
                        })
                        .await
                        .context("failed to write fixture session-closed response")?;
                    return Ok(());
                }
            },
        }
    }
}

use std::ffi::OsStr;
use std::future::Future;

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
use codex_code_mode_protocol::host::RequestId;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
use codex_code_mode_protocol::host::SessionId;
use codex_code_mode_protocol::host::WireCellId;
use codex_code_mode_protocol::host::WireContentItem;
use codex_code_mode_protocol::host::WireExecuteOutputPolicy;
use codex_code_mode_protocol::host::WireExecuteRequest;
use codex_code_mode_protocol::host::WireResult;
use codex_code_mode_protocol::host::WireRuntimeResponse;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

pub(super) const NO_SAVED_CAPABILITIES_MODE: &str = "no-saved-capabilities";
pub(super) const LEGACY_OUTPUT_ONLY_MODE: &str = "legacy-output-only";
pub(super) const INVALID_WORKFLOW_CAPABILITIES_MODE: &str = "invalid-workflow-capabilities";

const ORDINARY_CELL_ID: &str = "1";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NegotiationMode {
    NoSavedCapabilities,
    LegacyOutputOnly,
    InvalidWorkflowCapabilities,
}

impl NegotiationMode {
    pub(super) fn from_executable_stem(executable_stem: &OsStr) -> Option<Self> {
        if executable_stem == OsStr::new(NO_SAVED_CAPABILITIES_MODE) {
            Some(Self::NoSavedCapabilities)
        } else if executable_stem == OsStr::new(LEGACY_OUTPUT_ONLY_MODE) {
            Some(Self::LegacyOutputOnly)
        } else if executable_stem == OsStr::new(INVALID_WORKFLOW_CAPABILITIES_MODE) {
            Some(Self::InvalidWorkflowCapabilities)
        } else {
            None
        }
    }

    fn selected_capabilities(self) -> Result<CapabilitySet> {
        match self {
            Self::NoSavedCapabilities => Ok(CapabilitySet::empty()),
            Self::LegacyOutputOnly => Ok(CapabilitySet::try_new([Capability::new(
                SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY,
            )?])?),
            Self::InvalidWorkflowCapabilities => Ok(CapabilitySet::try_new([Capability::new(
                SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY,
            )?])?),
        }
    }

    fn unexpected_saved_execute(self) -> &'static str {
        match self {
            Self::NoSavedCapabilities => "no-capability fixture received a Saved execute",
            Self::LegacyOutputOnly => "legacy output-only fixture received a Saved execute",
            Self::InvalidWorkflowCapabilities => {
                "invalid-capability fixture entered its operation loop"
            }
        }
    }
}

/// Handles a Saved execute after the shared fixture loop has associated it with
/// an open session. Implementations emit any response themselves and return
/// only when the fixture connection should close.
pub(super) trait SavedExecuteHandler {
    fn handle<'a, R, W>(
        &'a self,
        id: RequestId,
        request: WireExecuteRequest,
        reader: &'a mut FramedReader<R>,
        writer: &'a mut FramedWriter<W>,
    ) -> impl Future<Output = Result<()>> + Send + 'a
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a;
}

struct RejectSavedExecute {
    message: &'static str,
}

impl SavedExecuteHandler for RejectSavedExecute {
    fn handle<'a, R, W>(
        &'a self,
        _id: RequestId,
        _request: WireExecuteRequest,
        _reader: &'a mut FramedReader<R>,
        _writer: &'a mut FramedWriter<W>,
    ) -> impl Future<Output = Result<()>> + Send + 'a
    where
        R: AsyncRead + Unpin + Send + 'a,
        W: AsyncWrite + Unpin + Send + 'a,
    {
        let message = self.message;
        async move { bail!("{message}") }
    }
}

pub(super) async fn run_negotiation_fixture<R, W>(
    mode: NegotiationMode,
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
{
    let selected_capabilities = mode.selected_capabilities()?;
    if mode == NegotiationMode::InvalidWorkflowCapabilities {
        negotiate(&mut reader, &mut writer, selected_capabilities).await?;
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

    run_fixture(
        reader,
        writer,
        selected_capabilities,
        RejectSavedExecute {
            message: mode.unexpected_saved_execute(),
        },
    )
    .await
}

pub(super) async fn run_fixture<R, W, H>(
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    selected_capabilities: CapabilitySet,
    saved_execute_handler: H,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
    H: SavedExecuteHandler,
{
    negotiate(&mut reader, &mut writer, selected_capabilities).await?;
    run_session(reader, writer, saved_execute_handler).await
}

async fn negotiate<R, W>(
    reader: &mut FramedReader<R>,
    writer: &mut FramedWriter<W>,
    selected_capabilities: CapabilitySet,
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
    Ok(())
}

async fn run_session<R, W, H>(
    mut reader: FramedReader<R>,
    mut writer: FramedWriter<W>,
    saved_execute_handler: H,
) -> Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send,
    H: SavedExecuteHandler,
{
    let mut opened_session: Option<SessionId> = None;
    loop {
        let Some(message) = reader
            .read::<ClientToHost>()
            .await
            .context("failed to read fixture client message")?
        else {
            ensure!(
                opened_session.is_none(),
                "fixture client closed before shutting down its session"
            );
            return Ok(());
        };
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
                        WireExecuteOutputPolicy::SavedWorkflow => {
                            return saved_execute_handler
                                .handle(id, request, &mut reader, &mut writer)
                                .await;
                        }
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
                    opened_session = None;
                }
            },
        }
    }
}

use codex_code_mode_protocol::host::Capability;
use codex_code_mode_protocol::host::CapabilitySet;
use codex_code_mode_protocol::host::ClientHello;
use codex_code_mode_protocol::host::ClientToHost;
use codex_code_mode_protocol::host::FramedReader;
use codex_code_mode_protocol::host::FramedWriter;
use codex_code_mode_protocol::host::HostToClient;
use codex_code_mode_protocol::host::ProtocolVersion;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
use codex_code_mode_protocol::host::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
use codex_code_mode_protocol::host::SupportedProtocolVersions;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

const INVALID_CLIENT_CAPABILITIES: &str = "code-mode client capabilities are invalid";
const HOST_SELECTED_UNSUPPORTED_VERSION: &str =
    "code-mode host selected an unsupported protocol version";
const HOST_SELECTED_UNOFFERED_CAPABILITY: &str = "code-mode host selected an unoffered capability";
const HOST_SELECTED_INVALID_WORKFLOW_CAPABILITIES: &str =
    "code-mode host selected an invalid workflow capability set";
const HOST_REJECTED_HANDSHAKE: &str = "code-mode host rejected the handshake";
const HOST_RETURNED_INVALID_HANDSHAKE: &str =
    "code-mode host returned an invalid handshake response";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct NegotiatedCapabilities {
    selected: CapabilitySet,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NegotiatedWorkflowCellIdentity {
    Unavailable,
    V1,
}

impl NegotiatedCapabilities {
    pub(super) fn try_from_selected(selected: CapabilitySet) -> Result<Self, String> {
        let output = selected.contains_name(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY);
        let identity = selected.contains_name(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY);
        if identity && !output {
            Err(HOST_SELECTED_INVALID_WORKFLOW_CAPABILITIES.to_string())
        } else {
            Ok(Self { selected })
        }
    }

    pub(super) fn selected(&self) -> &CapabilitySet {
        &self.selected
    }

    pub(super) fn workflow_cell_identity(&self) -> NegotiatedWorkflowCellIdentity {
        let output = self
            .selected
            .contains_name(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY);
        let identity = self
            .selected
            .contains_name(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY);
        if output && identity {
            NegotiatedWorkflowCellIdentity::V1
        } else {
            NegotiatedWorkflowCellIdentity::Unavailable
        }
    }
}

pub(super) async fn negotiate<R, W>(
    reader: &mut FramedReader<R>,
    writer: &mut FramedWriter<W>,
) -> Result<NegotiatedCapabilities, String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let saved_workflow_output = Capability::new(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY)
        .map_err(|_| INVALID_CLIENT_CAPABILITIES.to_string())?;
    let saved_workflow_cell_identity = Capability::new(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY)
        .map_err(|_| INVALID_CLIENT_CAPABILITIES.to_string())?;
    let required_capabilities = CapabilitySet::empty();
    let optional_capabilities =
        CapabilitySet::try_new([saved_workflow_output, saved_workflow_cell_identity])
            .map_err(|_| INVALID_CLIENT_CAPABILITIES.to_string())?;
    let hello = ClientHello::new(
        SupportedProtocolVersions::try_new([ProtocolVersion::V2]).map_err(|err| err.to_string())?,
        required_capabilities.clone(),
        optional_capabilities.clone(),
    )
    .map_err(|err| err.to_string())?;
    writer
        .write(&ClientToHost::ClientHello(hello))
        .await
        .map_err(|err| format!("failed to write code-mode host hello: {err}"))?;
    match reader
        .read::<HostToClient>()
        .await
        .map_err(|err| format!("failed to read code-mode host hello: {err}"))?
    {
        Some(HostToClient::HostHello(hello)) => {
            if hello.selected_version() != ProtocolVersion::V2 {
                return Err(HOST_SELECTED_UNSUPPORTED_VERSION.to_string());
            }
            if hello.capabilities().iter().any(|capability| {
                !required_capabilities.contains(capability)
                    && !optional_capabilities.contains(capability)
            }) {
                return Err(HOST_SELECTED_UNOFFERED_CAPABILITY.to_string());
            }
            NegotiatedCapabilities::try_from_selected(hello.capabilities().clone())
        }
        Some(HostToClient::HandshakeRejected { .. }) => Err(HOST_REJECTED_HANDSHAKE.to_string()),
        Some(_) => Err(HOST_RETURNED_INVALID_HANDSHAKE.to_string()),
        None => Err("code-mode host exited during handshake".to_string()),
    }
}

#[cfg(test)]
#[path = "handshake_tests.rs"]
mod tests;

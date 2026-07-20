use std::fmt;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use super::Capability;
use super::CapabilitySet;
use super::DelegateRequestId;
use super::HandshakeRejectReason;
use super::ProtocolVersion;
use super::RequestId;
use super::SessionId;
use super::SupportedProtocolVersions;
use super::WireAgentSpawnOutcome;
use super::WireCellId;
use super::WireExecuteRequest;
use super::WireNestedToolCall;
use super::WireRuntimeResponse;
use super::WireWaitOutcome;
use super::WireWaitRequest;
use crate::AgentCallOpts;
use crate::WorkflowBudgetSnapshot;
use crate::WorkflowHostProgress;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientHello {
    supported_versions: SupportedProtocolVersions,
    required_capabilities: CapabilitySet,
    optional_capabilities: CapabilitySet,
}

impl ClientHello {
    pub fn new(
        supported_versions: SupportedProtocolVersions,
        required_capabilities: CapabilitySet,
        optional_capabilities: CapabilitySet,
    ) -> Result<Self, ClientHelloError> {
        if let Some(capability) = required_capabilities
            .iter()
            .find(|capability| optional_capabilities.contains(capability))
        {
            return Err(ClientHelloError::OverlappingCapability(capability.clone()));
        }
        Ok(Self {
            supported_versions,
            required_capabilities,
            optional_capabilities,
        })
    }

    pub fn supported_versions(&self) -> &SupportedProtocolVersions {
        &self.supported_versions
    }

    pub fn required_capabilities(&self) -> &CapabilitySet {
        &self.required_capabilities
    }

    pub fn optional_capabilities(&self) -> &CapabilitySet {
        &self.optional_capabilities
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ClientHelloWire {
    supported_versions: SupportedProtocolVersions,
    required_capabilities: CapabilitySet,
    optional_capabilities: CapabilitySet,
}

impl<'de> Deserialize<'de> for ClientHello {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let wire = ClientHelloWire::deserialize(deserializer)?;
        Self::new(
            wire.supported_versions,
            wire.required_capabilities,
            wire.optional_capabilities,
        )
        .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientHelloError {
    OverlappingCapability(Capability),
}

impl fmt::Display for ClientHelloError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverlappingCapability(capability) => write!(
                formatter,
                "capability `{capability}` cannot be both required and optional"
            ),
        }
    }
}

impl std::error::Error for ClientHelloError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct HostHello {
    selected_version: ProtocolVersion,
    capabilities: CapabilitySet,
}

impl HostHello {
    pub fn new(selected_version: ProtocolVersion, capabilities: CapabilitySet) -> Self {
        Self {
            selected_version,
            capabilities,
        }
    }

    pub fn selected_version(&self) -> ProtocolVersion {
        self.selected_version
    }

    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }
}

/// Messages sent from a client to the code-mode host.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum ClientToHost {
    #[serde(rename = "connection/hello")]
    ClientHello(ClientHello),
    #[serde(rename = "operation/request")]
    Request { id: RequestId, request: HostRequest },
    #[serde(rename = "operation/cancel")]
    CancelRequest { id: RequestId },
    #[serde(rename = "delegate/response")]
    DelegateResponse {
        id: DelegateRequestId,
        result: WireResult<DelegateResponse>,
    },
}

/// Messages sent from the code-mode host to a client.
#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum HostToClient {
    #[serde(rename = "connection/ready")]
    HostHello(HostHello),
    #[serde(rename = "connection/rejected")]
    HandshakeRejected { reason: HandshakeRejectReason },
    #[serde(rename = "operation/response")]
    Response {
        id: RequestId,
        result: WireResult<HostResponse>,
    },
    #[serde(rename = "execute/initialResponse")]
    InitialResponse {
        id: RequestId,
        result: WireResult<WireRuntimeResponse>,
    },
    #[serde(rename = "delegate/request")]
    DelegateRequest {
        id: DelegateRequestId,
        session_id: SessionId,
        request: DelegateRequest,
    },
    #[serde(rename = "delegate/cancel")]
    CancelDelegateRequest { id: DelegateRequestId },
    #[serde(rename = "cell/closed")]
    CellClosed {
        session_id: SessionId,
        cell_id: WireCellId,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "method", rename_all_fields = "camelCase")]
pub enum HostRequest {
    #[serde(rename = "session/open")]
    OpenSession { session_id: SessionId },
    #[serde(rename = "session/execute")]
    Execute {
        session_id: SessionId,
        request: WireExecuteRequest,
    },
    #[serde(rename = "session/wait")]
    Wait {
        session_id: SessionId,
        request: WireWaitRequest,
    },
    #[serde(rename = "session/terminate")]
    Terminate {
        session_id: SessionId,
        cell_id: WireCellId,
    },
    #[serde(rename = "session/shutdown")]
    ShutdownSession { session_id: SessionId },
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum HostResponse {
    #[serde(rename = "session/ready")]
    SessionReady { session_id: SessionId },
    #[serde(rename = "execution/started")]
    ExecutionStarted { cell_id: WireCellId },
    #[serde(rename = "wait/completed")]
    WaitCompleted { outcome: WireWaitOutcome },
    #[serde(rename = "session/closed")]
    SessionClosed { session_id: SessionId },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum DelegateRequest {
    #[serde(rename = "tool/invoke")]
    InvokeTool { invocation: WireNestedToolCall },
    #[serde(rename = "notification/send")]
    Notify {
        call_id: String,
        cell_id: WireCellId,
        text: String,
    },
    /// A workflow `agent(prompt, opts?)` spawn. `cell_id` routes the call to the owning cell's local
    /// delegate (the real core spawn broker) just like [`Self::Notify`]; `ordinal` is the
    /// deterministic source-order invocation ordinal used host-side for a replay-stable subagent
    /// nickname.
    #[serde(rename = "agent/spawn")]
    SpawnAgent {
        cell_id: WireCellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        prompt: String,
        ordinal: u64,
        // Boxed to keep the (otherwise small) `DelegateRequest` — and every enum that embeds it —
        // from being dominated by the wide `AgentCallOpts`; serde treats `Box<T>` transparently, so
        // the wire shape is unchanged.
        opts: Box<AgentCallOpts>,
    },
    /// A workflow `workflow(nameOrRef, args)` nested run. `cell_id` routes the call to the owning
    /// cell's local delegate (the real core workflow handler) just like [`Self::SpawnAgent`]; `name`
    /// is the caller-supplied `nameOrRef` resolved against the registry client-side and `args` is the
    /// invocation JSON injected as the nested run's read-only `args` global.
    #[serde(rename = "workflow/spawn")]
    SpawnWorkflow {
        cell_id: WireCellId,
        name: String,
        args: Option<JsonValue>,
    },
    /// Persist a workflow phase marker in the client-owned run journal.
    #[serde(rename = "workflow/phase")]
    JournalPhase { cell_id: WireCellId, title: String },
    /// Persist a workflow log marker in the client-owned run journal.
    #[serde(rename = "workflow/log")]
    JournalLog {
        cell_id: WireCellId,
        message: String,
    },
    /// Re-append a prefix-replayed agent entry and restore its metered spend in
    /// the client-owned workflow state.
    #[serde(rename = "workflow/replayAgent")]
    ReplayAgent {
        cell_id: WireCellId,
        node_id: u64,
        parent_node_id: Option<u64>,
        phase: Option<String>,
        entry: JsonValue,
    },
    /// Source-ordered workflow progress and terminal runtime state. Core remains the only layer
    /// that publishes these as durable protocol events.
    #[serde(rename = "workflow/progress")]
    WorkflowProgress {
        cell_id: WireCellId,
        progress: Box<WorkflowHostProgress>,
    },
    /// Read the current run-local budget after a budget-affecting callback.
    #[serde(rename = "workflow/budget")]
    WorkflowBudget { cell_id: WireCellId },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all_fields = "camelCase")]
pub enum DelegateResponse {
    #[serde(rename = "tool/result")]
    ToolResult { result: JsonValue },
    #[serde(rename = "notification/delivered")]
    NotificationDelivered,
    /// The resolution of a [`DelegateRequest::SpawnAgent`]: the three-way `agent()` outcome
    /// (resolve-with-value / resolve-to-null / reject) round-tripped back to the host isolate.
    #[serde(rename = "agent/spawned")]
    AgentSpawned { outcome: WireAgentSpawnOutcome },
    /// The resolution of a [`DelegateRequest::SpawnWorkflow`]: the nested `workflow()` run's outcome
    /// (resolve-with-value / resolve-to-null / reject) round-tripped back to the host isolate. Reuses
    /// [`WireAgentSpawnOutcome`] because the three-way settlement is identical to `agent()`.
    #[serde(rename = "workflow/spawned")]
    WorkflowSpawned { outcome: WireAgentSpawnOutcome },
    /// Current run-local budget view for the requesting cell.
    #[serde(rename = "workflow/budget")]
    WorkflowBudget {
        snapshot: Option<WorkflowBudgetSnapshot>,
    },
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "status", rename_all_fields = "camelCase")]
pub enum WireResult<T> {
    #[serde(rename = "ok")]
    Ok { value: T },
    #[serde(rename = "error")]
    Err { message: String },
}

impl<T> WireResult<T> {
    pub fn from_result(result: Result<T, String>) -> Self {
        match result {
            Ok(value) => Self::Ok { value },
            Err(message) => Self::Err { message },
        }
    }

    pub fn into_result(self) -> Result<T, String> {
        match self {
            Self::Ok { value } => Ok(value),
            Self::Err { message } => Err(message),
        }
    }
}

use std::fmt;
use std::num::TryFromIntError;

use codex_protocol::ToolName;
use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Error as _;
use serde::ser::Error as _;
use serde_json::Value as JsonValue;

use super::types::CapabilitySet;
use super::types::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
use super::types::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
use super::workflow_cell_id::WireWorkflowCellId;
use crate::CellId;
use crate::CodeModeNestedToolCall;
use crate::CodeModeToolKind;
use crate::ExecuteOutputPolicy;
use crate::ExecuteRequest;
use crate::FunctionCallOutputContentItem;
use crate::ImageDetail;
use crate::RuntimeResponse;
use crate::ToolDefinition;
use crate::WaitOutcome;
use crate::WaitRequest;

/// A cell identifier with a wire representation owned by protocol V2.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct WireCellId(String);

/// Maximum UTF-8 byte length of one protocol V2 cell identifier.
pub const WIRE_CELL_ID_MAX_BYTES: usize = 256;

/// Failure returned for an empty, oversized, or control-bearing protocol V2 cell identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidWireCellId;

impl fmt::Display for InvalidWireCellId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("invalid code-mode cell ID")
    }
}

impl std::error::Error for InvalidWireCellId {}

impl WireCellId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn try_new(value: impl Into<String>) -> Result<Self, InvalidWireCellId> {
        let value = value.into();
        ensure_valid_wire_cell_id(&value)?;
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn validate(&self) -> Result<(), InvalidWireCellId> {
        ensure_valid_wire_cell_id(&self.0)
    }
}

impl Serialize for WireCellId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(S::Error::custom)?;
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for WireCellId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::try_new(String::deserialize(deserializer)?).map_err(D::Error::custom)
    }
}

fn ensure_valid_wire_cell_id(value: &str) -> Result<(), InvalidWireCellId> {
    if value.is_empty()
        || value.len() > WIRE_CELL_ID_MAX_BYTES
        || value.chars().any(char::is_control)
    {
        return Err(InvalidWireCellId);
    }
    Ok(())
}

impl From<CellId> for WireCellId {
    fn from(value: CellId) -> Self {
        Self(value.as_str().to_string())
    }
}

impl From<&CellId> for WireCellId {
    fn from(value: &CellId) -> Self {
        Self(value.as_str().to_string())
    }
}

impl From<WireCellId> for CellId {
    fn from(value: WireCellId) -> Self {
        Self::new(value.0)
    }
}

/// The V2 wire representation of a tool's stable name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolName {
    pub name: String,
    pub namespace: Option<String>,
}

impl From<ToolName> for WireToolName {
    fn from(value: ToolName) -> Self {
        Self {
            name: value.name,
            namespace: value.namespace,
        }
    }
}

impl From<WireToolName> for ToolName {
    fn from(value: WireToolName) -> Self {
        Self::new(value.namespace, value.name)
    }
}

/// The tool invocation shape supported by protocol V2.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireToolKind {
    Function,
    Freeform,
}

impl From<CodeModeToolKind> for WireToolKind {
    fn from(value: CodeModeToolKind) -> Self {
        match value {
            CodeModeToolKind::Function => Self::Function,
            CodeModeToolKind::Freeform => Self::Freeform,
        }
    }
}

impl From<WireToolKind> for CodeModeToolKind {
    fn from(value: WireToolKind) -> Self {
        match value {
            WireToolKind::Function => Self::Function,
            WireToolKind::Freeform => Self::Freeform,
        }
    }
}

/// A V2 tool definition embedded in an execute request.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolDefinition {
    pub name: String,
    pub tool_name: WireToolName,
    pub description: String,
    pub kind: WireToolKind,
    pub input_schema: Option<JsonValue>,
    pub output_schema: Option<JsonValue>,
}

impl From<ToolDefinition> for WireToolDefinition {
    fn from(value: ToolDefinition) -> Self {
        Self {
            name: value.name,
            tool_name: value.tool_name.into(),
            description: value.description,
            kind: value.kind.into(),
            input_schema: value.input_schema,
            output_schema: value.output_schema,
        }
    }
}

impl From<WireToolDefinition> for ToolDefinition {
    fn from(value: WireToolDefinition) -> Self {
        Self {
            name: value.name,
            tool_name: value.tool_name.into(),
            description: value.description,
            kind: value.kind.into(),
            input_schema: value.input_schema,
            output_schema: value.output_schema,
        }
    }
}

/// Selects the output contract encoded in a protocol V2 execute request.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireExecuteOutputPolicy {
    #[default]
    Ordinary,
    SavedWorkflow,
}

impl WireExecuteOutputPolicy {
    fn is_ordinary(&self) -> bool {
        *self == Self::Ordinary
    }
}

/// The complete execute request shape supported by protocol V2.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireExecuteRequest {
    pub tool_call_id: String,
    pub enabled_tools: Vec<WireToolDefinition>,
    pub source: String,
    #[serde(default, skip_serializing_if = "WireExecuteOutputPolicy::is_ordinary")]
    pub output_policy: WireExecuteOutputPolicy,
    /// Present only when both Saved-workflow capabilities are selected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_cell_id: Option<WireWorkflowCellId>,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<i32>,
}

/// Describes who chooses the cell identifier for one decoded execute request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireExecuteCellIdentity {
    /// The host retains its existing cell-ID allocator.
    HostAllocated,
    /// The host must adopt the exact client-assigned Saved-workflow ID.
    SavedWorkflow(WireWorkflowCellId),
}

/// A capability-validated execute request and its cell-identity contract.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodedWireExecuteRequest {
    pub request: ExecuteRequest,
    pub cell_identity: WireExecuteCellIdentity,
}

/// Failure converting between domain and protocol V2 execute requests.
#[derive(Debug)]
pub enum WireExecuteRequestConversionError {
    SavedWorkflowOutputPolicyUnavailable,
    SavedWorkflowCellIdentityUnavailable,
    SavedWorkflowCellIdentityRequired,
    WorkflowCellIdentityRequiresSavedWorkflow,
    MaxOutputTokensOutOfRange(TryFromIntError),
}

impl fmt::Display for WireExecuteRequestConversionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SavedWorkflowOutputPolicyUnavailable => {
                formatter.write_str(crate::SAVED_WORKFLOW_OUTPUT_POLICY_UNAVAILABLE)
            }
            Self::SavedWorkflowCellIdentityUnavailable => {
                formatter.write_str("saved workflow cell identity is unavailable")
            }
            Self::SavedWorkflowCellIdentityRequired => {
                formatter.write_str("saved workflow cell identity is required")
            }
            Self::WorkflowCellIdentityRequiresSavedWorkflow => {
                formatter.write_str("workflow cell identity requires saved workflow output")
            }
            Self::MaxOutputTokensOutOfRange(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for WireExecuteRequestConversionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::SavedWorkflowOutputPolicyUnavailable
            | Self::SavedWorkflowCellIdentityUnavailable
            | Self::SavedWorkflowCellIdentityRequired
            | Self::WorkflowCellIdentityRequiresSavedWorkflow => None,
            Self::MaxOutputTokensOutOfRange(error) => Some(error),
        }
    }
}

impl From<TryFromIntError> for WireExecuteRequestConversionError {
    fn from(error: TryFromIntError) -> Self {
        Self::MaxOutputTokensOutOfRange(error)
    }
}

impl WireExecuteRequest {
    /// Converts a domain request using the capabilities selected for this connection.
    pub fn try_from_domain(
        value: ExecuteRequest,
        selected_capabilities: &CapabilitySet,
    ) -> Result<Self, WireExecuteRequestConversionError> {
        Self::try_from_domain_with_identity(
            value,
            selected_capabilities,
            WireExecuteCellIdentity::HostAllocated,
        )
    }

    /// Converts a Saved-workflow request with a client-assigned cell identifier.
    pub fn try_from_domain_with_workflow_cell_id(
        value: ExecuteRequest,
        selected_capabilities: &CapabilitySet,
        workflow_cell_id: WireWorkflowCellId,
    ) -> Result<Self, WireExecuteRequestConversionError> {
        Self::try_from_domain_with_identity(
            value,
            selected_capabilities,
            WireExecuteCellIdentity::SavedWorkflow(workflow_cell_id),
        )
    }

    fn try_from_domain_with_identity(
        value: ExecuteRequest,
        selected_capabilities: &CapabilitySet,
        cell_identity: WireExecuteCellIdentity,
    ) -> Result<Self, WireExecuteRequestConversionError> {
        let output_policy = match value.output_policy {
            ExecuteOutputPolicy::Ordinary => WireExecuteOutputPolicy::Ordinary,
            ExecuteOutputPolicy::SavedWorkflow
                if supports_saved_workflow_output(selected_capabilities) =>
            {
                WireExecuteOutputPolicy::SavedWorkflow
            }
            ExecuteOutputPolicy::SavedWorkflow => {
                return Err(
                    WireExecuteRequestConversionError::SavedWorkflowOutputPolicyUnavailable,
                );
            }
        };
        let workflow_cell_id =
            match validate_cell_identity(&output_policy, cell_identity, selected_capabilities)? {
                WireExecuteCellIdentity::HostAllocated => None,
                WireExecuteCellIdentity::SavedWorkflow(workflow_cell_id) => Some(workflow_cell_id),
            };
        Ok(Self {
            tool_call_id: value.tool_call_id,
            enabled_tools: value.enabled_tools.into_iter().map(Into::into).collect(),
            source: value.source,
            output_policy,
            workflow_cell_id,
            yield_time_ms: value.yield_time_ms,
            max_output_tokens: value.max_output_tokens.map(i32::try_from).transpose()?,
        })
    }

    /// Converts a wire request using the capabilities selected for this connection.
    pub fn try_into_domain(
        self,
        selected_capabilities: &CapabilitySet,
    ) -> Result<DecodedWireExecuteRequest, WireExecuteRequestConversionError> {
        let output_policy = match &self.output_policy {
            WireExecuteOutputPolicy::Ordinary => ExecuteOutputPolicy::Ordinary,
            WireExecuteOutputPolicy::SavedWorkflow
                if supports_saved_workflow_output(selected_capabilities) =>
            {
                ExecuteOutputPolicy::SavedWorkflow
            }
            WireExecuteOutputPolicy::SavedWorkflow => {
                return Err(
                    WireExecuteRequestConversionError::SavedWorkflowOutputPolicyUnavailable,
                );
            }
        };
        let cell_identity = validate_cell_identity(
            &self.output_policy,
            self.workflow_cell_id
                .map(WireExecuteCellIdentity::SavedWorkflow)
                .unwrap_or(WireExecuteCellIdentity::HostAllocated),
            selected_capabilities,
        )?;
        Ok(DecodedWireExecuteRequest {
            request: ExecuteRequest {
                tool_call_id: self.tool_call_id,
                enabled_tools: self.enabled_tools.into_iter().map(Into::into).collect(),
                source: self.source,
                output_policy,
                yield_time_ms: self.yield_time_ms,
                max_output_tokens: self.max_output_tokens.map(usize::try_from).transpose()?,
            },
            cell_identity,
        })
    }
}

fn validate_cell_identity(
    output_policy: &WireExecuteOutputPolicy,
    cell_identity: WireExecuteCellIdentity,
    selected_capabilities: &CapabilitySet,
) -> Result<WireExecuteCellIdentity, WireExecuteRequestConversionError> {
    match (output_policy, cell_identity) {
        (WireExecuteOutputPolicy::Ordinary, WireExecuteCellIdentity::HostAllocated) => {
            Ok(WireExecuteCellIdentity::HostAllocated)
        }
        (WireExecuteOutputPolicy::Ordinary, WireExecuteCellIdentity::SavedWorkflow(_)) => {
            Err(WireExecuteRequestConversionError::WorkflowCellIdentityRequiresSavedWorkflow)
        }
        (WireExecuteOutputPolicy::SavedWorkflow, WireExecuteCellIdentity::HostAllocated)
            if supports_saved_workflow_cell_identity(selected_capabilities) =>
        {
            Err(WireExecuteRequestConversionError::SavedWorkflowCellIdentityRequired)
        }
        (WireExecuteOutputPolicy::SavedWorkflow, WireExecuteCellIdentity::HostAllocated) => {
            Ok(WireExecuteCellIdentity::HostAllocated)
        }
        (
            WireExecuteOutputPolicy::SavedWorkflow,
            WireExecuteCellIdentity::SavedWorkflow(workflow_cell_id),
        ) if supports_saved_workflow_cell_identity(selected_capabilities) => {
            Ok(WireExecuteCellIdentity::SavedWorkflow(workflow_cell_id))
        }
        (WireExecuteOutputPolicy::SavedWorkflow, WireExecuteCellIdentity::SavedWorkflow(_)) => {
            Err(WireExecuteRequestConversionError::SavedWorkflowCellIdentityUnavailable)
        }
    }
}

fn supports_saved_workflow_output(selected_capabilities: &CapabilitySet) -> bool {
    selected_capabilities.contains_name(SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY)
}

fn supports_saved_workflow_cell_identity(selected_capabilities: &CapabilitySet) -> bool {
    selected_capabilities.contains_name(SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY)
}

impl TryFrom<ExecuteRequest> for WireExecuteRequest {
    type Error = WireExecuteRequestConversionError;

    fn try_from(value: ExecuteRequest) -> Result<Self, Self::Error> {
        Self::try_from_domain(value, &CapabilitySet::empty())
    }
}

impl TryFrom<WireExecuteRequest> for ExecuteRequest {
    type Error = WireExecuteRequestConversionError;

    fn try_from(value: WireExecuteRequest) -> Result<Self, Self::Error> {
        Ok(value.try_into_domain(&CapabilitySet::empty())?.request)
    }
}

/// The complete wait request shape supported by protocol V2.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireWaitRequest {
    pub cell_id: WireCellId,
    pub yield_time_ms: u64,
}

impl From<WaitRequest> for WireWaitRequest {
    fn from(value: WaitRequest) -> Self {
        Self {
            cell_id: value.cell_id.into(),
            yield_time_ms: value.yield_time_ms,
        }
    }
}

impl From<WireWaitRequest> for WaitRequest {
    fn from(value: WireWaitRequest) -> Self {
        Self {
            cell_id: value.cell_id.into(),
            yield_time_ms: value.yield_time_ms,
        }
    }
}

/// Image detail values accepted in a V2 runtime response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WireImageDetail {
    Auto,
    Low,
    High,
    Original,
}

impl From<ImageDetail> for WireImageDetail {
    fn from(value: ImageDetail) -> Self {
        match value {
            ImageDetail::Auto => Self::Auto,
            ImageDetail::Low => Self::Low,
            ImageDetail::High => Self::High,
            ImageDetail::Original => Self::Original,
        }
    }
}

impl From<WireImageDetail> for ImageDetail {
    fn from(value: WireImageDetail) -> Self {
        match value {
            WireImageDetail::Auto => Self::Auto,
            WireImageDetail::Low => Self::Low,
            WireImageDetail::High => Self::High,
            WireImageDetail::Original => Self::Original,
        }
    }
}

/// One output item emitted by a V2 runtime response.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields, tag = "type", rename_all = "snake_case")]
pub enum WireContentItem {
    InputText {
        text: String,
    },
    InputImage {
        image_url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<WireImageDetail>,
    },
    InputAudio {
        audio_url: String,
    },
}

impl From<FunctionCallOutputContentItem> for WireContentItem {
    fn from(value: FunctionCallOutputContentItem) -> Self {
        match value {
            FunctionCallOutputContentItem::InputText { text } => Self::InputText { text },
            FunctionCallOutputContentItem::InputImage { image_url, detail } => Self::InputImage {
                image_url,
                detail: detail.map(Into::into),
            },
            FunctionCallOutputContentItem::InputAudio { audio_url } => {
                Self::InputAudio { audio_url }
            }
        }
    }
}

impl From<WireContentItem> for FunctionCallOutputContentItem {
    fn from(value: WireContentItem) -> Self {
        match value {
            WireContentItem::InputText { text } => Self::InputText { text },
            WireContentItem::InputImage { image_url, detail } => Self::InputImage {
                image_url,
                detail: detail.map(Into::into),
            },
            WireContentItem::InputAudio { audio_url } => Self::InputAudio { audio_url },
        }
    }
}

/// Runtime output returned over the V2 host connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum WireRuntimeResponse {
    Yielded {
        cell_id: WireCellId,
        content_items: Vec<WireContentItem>,
    },
    Terminated {
        cell_id: WireCellId,
        content_items: Vec<WireContentItem>,
    },
    Result {
        cell_id: WireCellId,
        content_items: Vec<WireContentItem>,
        error_text: Option<String>,
    },
}

impl From<RuntimeResponse> for WireRuntimeResponse {
    fn from(value: RuntimeResponse) -> Self {
        match value {
            RuntimeResponse::Yielded {
                cell_id,
                content_items,
            } => Self::Yielded {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            RuntimeResponse::Terminated {
                cell_id,
                content_items,
            } => Self::Terminated {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            RuntimeResponse::Result {
                cell_id,
                content_items,
                error_text,
            } => Self::Result {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
                error_text,
            },
        }
    }
}

impl From<WireRuntimeResponse> for RuntimeResponse {
    fn from(value: WireRuntimeResponse) -> Self {
        match value {
            WireRuntimeResponse::Yielded {
                cell_id,
                content_items,
            } => Self::Yielded {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            WireRuntimeResponse::Terminated {
                cell_id,
                content_items,
            } => Self::Terminated {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
            },
            WireRuntimeResponse::Result {
                cell_id,
                content_items,
                error_text,
            } => Self::Result {
                cell_id: cell_id.into(),
                content_items: content_items.into_iter().map(Into::into).collect(),
                error_text,
            },
        }
    }
}

/// Whether a waited-for cell remained live in protocol V2.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub enum WireWaitOutcome {
    LiveCell(WireRuntimeResponse),
    MissingCell(WireRuntimeResponse),
}

impl From<WaitOutcome> for WireWaitOutcome {
    fn from(value: WaitOutcome) -> Self {
        match value {
            WaitOutcome::LiveCell(response) => Self::LiveCell(response.into()),
            WaitOutcome::MissingCell(response) => Self::MissingCell(response.into()),
        }
    }
}

impl From<WireWaitOutcome> for WaitOutcome {
    fn from(value: WireWaitOutcome) -> Self {
        match value {
            WireWaitOutcome::LiveCell(response) => Self::LiveCell(response.into()),
            WireWaitOutcome::MissingCell(response) => Self::MissingCell(response.into()),
        }
    }
}

/// A nested tool invocation sent over the V2 host connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireNestedToolCall {
    pub cell_id: WireCellId,
    pub runtime_tool_call_id: String,
    pub tool_name: WireToolName,
    pub tool_kind: WireToolKind,
    pub input: Option<JsonValue>,
}

impl From<CodeModeNestedToolCall> for WireNestedToolCall {
    fn from(value: CodeModeNestedToolCall) -> Self {
        Self {
            cell_id: value.cell_id.into(),
            runtime_tool_call_id: value.runtime_tool_call_id,
            tool_name: value.tool_name.into(),
            tool_kind: value.tool_kind.into(),
            input: value.input,
        }
    }
}

impl From<WireNestedToolCall> for CodeModeNestedToolCall {
    fn from(value: WireNestedToolCall) -> Self {
        Self {
            cell_id: value.cell_id.into(),
            runtime_tool_call_id: value.runtime_tool_call_id,
            tool_name: value.tool_name.into(),
            tool_kind: value.tool_kind.into(),
            input: value.input,
        }
    }
}

#[cfg(test)]
#[path = "payload_tests.rs"]
mod tests;

use codex_protocol::ToolName;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::CellId;
use crate::CodeModeToolKind;
use crate::FunctionCallOutputContentItem;
use crate::ToolDefinition;

pub const DEFAULT_EXEC_YIELD_TIME_MS: u64 = 10_000;
pub const DEFAULT_WAIT_YIELD_TIME_MS: u64 = 10_000;
pub const DEFAULT_MAX_OUTPUT_TOKENS_PER_EXEC_CALL: usize = 10_000;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExecuteRequest {
    pub tool_call_id: String,
    pub enabled_tools: Vec<ToolDefinition>,
    pub source: String,
    pub yield_time_ms: Option<u64>,
    pub max_output_tokens: Option<usize>,
    /// Explicit invocation mode: `true` only when the request originates from the
    /// workflow handler, which authorizes the workflow-only narrator globals
    /// (`phase`/`log`, and future `agent`/`args`/`budget`). Plain code-mode
    /// `exec` never sets this, so a program whose source merely *looks* like a
    /// workflow (e.g. contains `export const meta = { ... }`) does not gain the
    /// workflow globals. Serde-defaulted to `false` for backward compatibility
    /// with older wire payloads that predate the field. Additionally skipped when
    /// `false` so a plain code-mode exec serializes byte-identically to the
    /// pre-`workflow` wire format — critical for new-client -> old-host V1 hosts
    /// that use `deny_unknown_fields` and would otherwise reject an unknown key.
    #[serde(default, skip_serializing_if = "is_false")]
    pub workflow: bool,
}

/// Serde predicate: skip a `bool` field when it is `false`. Takes `&bool`
/// because `skip_serializing_if` requires a `fn(&T) -> bool` signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WaitRequest {
    pub cell_id: CellId,
    pub yield_time_ms: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WaitToPendingRequest {
    pub cell_id: CellId,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum WaitOutcome {
    LiveCell(RuntimeResponse),
    MissingCell(RuntimeResponse),
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum ExecuteToPendingOutcome {
    Pending {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
        pending_tool_call_ids: Vec<String>,
    },
    Completed(RuntimeResponse),
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub enum WaitToPendingOutcome {
    LiveCell(ExecuteToPendingOutcome),
    MissingCell(RuntimeResponse),
}

impl From<WaitOutcome> for RuntimeResponse {
    fn from(outcome: WaitOutcome) -> Self {
        match outcome {
            WaitOutcome::LiveCell(response) | WaitOutcome::MissingCell(response) => response,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub enum RuntimeResponse {
    Yielded {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
    },
    Terminated {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
    },
    Result {
        cell_id: CellId,
        content_items: Vec<FunctionCallOutputContentItem>,
        error_text: Option<String>,
    },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CodeModeNestedToolCall {
    pub cell_id: CellId,
    pub runtime_tool_call_id: String,
    pub tool_name: ToolName,
    pub tool_kind: CodeModeToolKind,
    pub input: Option<JsonValue>,
}

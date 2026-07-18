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
    /// Invocation JSON injected read-only as the workflow `args` global via
    /// `json_to_v8` (§4), exactly as `build_tools_object` injects tool metadata.
    /// Set only by the workflow handler; serde-defaulted and skipped when absent
    /// so plain code-mode exec and pre-`args` peers serialize byte-identically to
    /// the older wire format. Installed as a global only for workflow runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<JsonValue>,
    /// Host-minted uuid v7 run identifier (§7), exposed read-only as
    /// `workflow.runId`. Minted in Rust with `uuid::Uuid::now_v7()` — never in the
    /// isolate — so the script can never derive ids/time/random itself.
    /// Serde-defaulted and skipped when absent for wire back-compat.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
}

/// Serde predicate: skip a `bool` field when it is `false`. Takes `&bool`
/// because `skip_serializing_if` requires a `fn(&T) -> bool` signature.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(value: &bool) -> bool {
    !*value
}

/// Typed payload for the `agent(prompt, opts?)` workflow global (§6 `agent()`
/// options). Mirrors the JS `opts` object documented in the spec: every field is
/// optional and unknown keys are **ignored** rather than rejected (no
/// `deny_unknown_fields`), so a workflow can pass forward-compatible extra keys
/// without breaking older hosts. This is the pure type surface both
/// `agent_callback` (which emits `RuntimeEvent::AgentCall`) and the cell-actor
/// spawn dispatch build against; no behavior lives here.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCallOpts {
    /// Progress-tree leaf label (cosmetic; excluded from the replay cache key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Progress grouping tag (cosmetic; excluded from the replay cache key).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// JSON Schema forcing a StructuredOutput final answer; the return is the
    /// validated parsed object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<JsonValue>,
    /// Model name resolved against `ModelsManager.list_models`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort (`'low'..'max'`) mapped to `ReasoningEffort`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// Isolation backend (only `'worktree'` today): run in a fresh git worktree
    /// with its own cwd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isolation: Option<String>,
    /// Agent role, resolved to a `role_name` via `apply_role_to_config`. Serde
    /// field name is `agentType` to match the JS opts key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::AgentCallOpts;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    /// A representative `agent()` opts object as it arrives from JS round-trips
    /// through [`AgentCallOpts`] and back to the same JSON, and unknown keys are
    /// ignored rather than rejected.
    #[test]
    fn agent_call_opts_round_trips_representative_js_opts() {
        let incoming = json!({
            "label": "file-a",
            "phase": "analyze",
            "schema": { "type": "object", "properties": { "ok": { "type": "boolean" } } },
            "model": "gpt-5-codex",
            "effort": "high",
            "isolation": "worktree",
            "agentType": "reviewer",
            // Forward-compatible extra key the host does not yet understand: it
            // must be ignored, not rejected (no `deny_unknown_fields`).
            "futureKnob": 42,
        });

        let opts: AgentCallOpts =
            serde_json::from_value(incoming.clone()).expect("opts must deserialize");

        assert_eq!(opts.label.as_deref(), Some("file-a"));
        assert_eq!(opts.phase.as_deref(), Some("analyze"));
        assert_eq!(opts.model.as_deref(), Some("gpt-5-codex"));
        assert_eq!(opts.effort.as_deref(), Some("high"));
        assert_eq!(opts.isolation.as_deref(), Some("worktree"));
        assert_eq!(opts.agent_type.as_deref(), Some("reviewer"));
        assert_eq!(
            opts.schema,
            Some(json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } }
            }))
        );

        // Serialize back: the known fields reproduce the original JS payload
        // (minus the ignored unknown key), with `agentType` restored via
        // camelCase renaming.
        let mut expected = incoming;
        expected
            .as_object_mut()
            .expect("json object")
            .remove("futureKnob");
        assert_eq!(serde_json::to_value(&opts).expect("serialize"), expected);
    }

    /// Every field is optional: an empty JS opts object yields an all-`None`
    /// value that serializes back to an empty object.
    #[test]
    fn agent_call_opts_all_fields_optional() {
        let opts: AgentCallOpts = serde_json::from_value(json!({})).expect("empty opts");
        assert_eq!(opts, AgentCallOpts::default());
        assert_eq!(serde_json::to_value(&opts).expect("serialize"), json!({}));
    }
}

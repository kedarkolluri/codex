use codex_core_workflows::WorkflowBudget;
use codex_core_workflows::WorkflowBudgetLimit;
use codex_features::Feature;
use codex_features::Features;

use crate::function_tool::FunctionCallError;

use super::super::WORKFLOW_TOOL_NAME;

/// Hard cap (in bytes) on any error string this handler surfaces to the model
/// as a tool result.
///
/// Defense-in-depth: even though [`codex_code_mode::parse_workflow_meta`] now
/// bounds the identifiers/numbers it echoes, an error can still originate from
/// several layers (the exec-source parser, the isolate service, the runtime
/// response adapter). No single error returned from the workflow tool path
/// should be able to balloon the model context, so every model-visible error is
/// hard-truncated at a UTF-8 boundary with a marker.
const MAX_MODEL_ERROR_BYTES: usize = 768;

/// Marker appended to a truncated error. Its byte length is reserved inside the
/// [`MAX_MODEL_ERROR_BYTES`] budget so the final string never exceeds the cap.
const ERROR_TRUNCATION_MARKER: &str = "… [error truncated]";

/// Truncate a model-visible error message so its total length never exceeds the
/// hard cap [`MAX_MODEL_ERROR_BYTES`]. The marker length is reserved inside the
/// budget (truncate to `cap - marker_len` at a UTF-8 char boundary), so the
/// returned string — prefix plus marker — is guaranteed `<= MAX_MODEL_ERROR_BYTES`.
pub(super) fn truncate_model_error(message: String) -> String {
    if message.len() <= MAX_MODEL_ERROR_BYTES {
        return message;
    }
    let mut end = MAX_MODEL_ERROR_BYTES.saturating_sub(ERROR_TRUNCATION_MARKER.len());
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{ERROR_TRUNCATION_MARKER}", &message[..end])
}

pub(in crate::tools::code_mode) fn ensure_workflow_args_within_bounds(
    args: &serde_json::Value,
) -> Result<(), FunctionCallError> {
    codex_code_mode::ensure_workflow_args(args).map_err(FunctionCallError::RespondToModel)
}

pub(in crate::tools::code_mode) fn ensure_model_workflow_args_within_bounds(
    args: &serde_json::Value,
) -> Result<(), FunctionCallError> {
    codex_code_mode::ensure_workflow_model_args(args).map_err(FunctionCallError::RespondToModel)
}

/// Hard-bound a [`FunctionCallError`] before it reaches the model. Only the
/// message payload is truncated; the error variant is preserved.
pub(super) fn bound_model_error(error: FunctionCallError) -> FunctionCallError {
    match error {
        FunctionCallError::RespondToModel(message) => {
            FunctionCallError::RespondToModel(truncate_model_error(message))
        }
        FunctionCallError::Fatal(message) => {
            FunctionCallError::Fatal(truncate_model_error(message))
        }
    }
}

/// Whether a workflow invocation declares a budget ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowBudgetSpec {
    /// No usable `budget.total`: the run is unmetered (the cell is reset).
    Absent,
    /// An explicit, non-negative ceiling. `0` is a real zero budget (reject the
    /// first `agent()`), not "unmetered".
    Limit(i64),
}

/// Classify `args.budget.total` into a [`WorkflowBudgetSpec`].
///
/// Distinguishing ABSENT (no `budget` key, or `budget` with no integer `total`)
/// from an explicit `total == 0` is the whole point: absent means unmetered,
/// while `{ budget: { total: 0 } }` is a real zero ceiling. A negative total is
/// invalid and clamped to `0` rather than silently treated as unmetered.
fn workflow_budget_spec(args: &serde_json::Value) -> WorkflowBudgetSpec {
    match args
        .get("budget")
        .and_then(|budget| budget.get("total"))
        .and_then(serde_json::Value::as_i64)
    {
        Some(total) => WorkflowBudgetSpec::Limit(total.max(0)),
        None => WorkflowBudgetSpec::Absent,
    }
}

pub(super) fn workflow_budget_limit(args: &serde_json::Value) -> WorkflowBudgetLimit {
    match workflow_budget_spec(args) {
        WorkflowBudgetSpec::Absent => WorkflowBudgetLimit::Unmetered,
        WorkflowBudgetSpec::Limit(total) => WorkflowBudgetLimit::Limited(total as u64),
    }
}

pub(in crate::tools::code_mode) fn protocol_budget_snapshot(
    budget: &WorkflowBudget,
) -> codex_code_mode::WorkflowBudgetSnapshot {
    let snapshot = budget.effective_snapshot();
    codex_code_mode::WorkflowBudgetSnapshot {
        total: match snapshot.limit {
            WorkflowBudgetLimit::Unmetered => None,
            WorkflowBudgetLimit::Limited(total) => Some(total),
        },
        spent: snapshot.spent,
        remaining: snapshot.remaining,
    }
}

/// Reject the call unless the `workflow` feature is enabled. This is what makes
/// the handler unreachable when [`Feature::Workflow`] is off.
pub(crate) fn ensure_workflow_enabled(features: &Features) -> Result<(), FunctionCallError> {
    if features.enabled(Feature::Workflow) {
        Ok(())
    } else {
        Err(FunctionCallError::RespondToModel(format!(
            "`{WORKFLOW_TOOL_NAME}` requires the `workflow` feature to be enabled"
        )))
    }
}

/// Validate the leading `export const meta = { ... }` manifest WITHOUT executing
/// (or even reading past) the workflow body.
#[cfg(test)]
pub(crate) fn validate_workflow_meta(code: &str) -> Result<(), FunctionCallError> {
    codex_code_mode::parse_workflow_meta(code)
        .map(|_meta| ())
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("invalid workflow `meta` manifest: {err}"))
        })
}

#[cfg(test)]
#[path = "bounds_tests.rs"]
mod tests;

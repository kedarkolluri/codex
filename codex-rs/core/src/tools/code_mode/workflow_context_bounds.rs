//! Bounds for workflow-authored data that can reach a child model context.

use codex_code_mode::ensure_workflow_agent_label;
use codex_code_mode::ensure_workflow_agent_option;
use codex_code_mode::ensure_workflow_agent_prompt;
use codex_code_mode::ensure_workflow_phase_title;
use codex_utils_output_truncation::approx_token_count;
use serde_json::Value as JsonValue;

/// A workflow `agent()` prompt becomes the child's first user input. Keep it below the repository's
/// 10K-token hard limit with a conservative safety margin for surrounding request structure.
pub(super) const WORKFLOW_AGENT_PROMPT_MAX_TOKENS: usize = 8 * 1024;

/// Reject an oversized `agent()` prompt before reserving budget, allocating a worktree, or spawning
/// a child. The repository-wide token estimator is deliberately used here so this admission check
/// follows the same accounting convention as other model-visible context bounds.
pub(super) fn ensure_prompt_within_bounds(prompt: &str) -> Result<(), String> {
    let estimated_tokens = approx_token_count(prompt);
    if estimated_tokens > WORKFLOW_AGENT_PROMPT_MAX_TOKENS {
        return Err(format!(
            "agent() prompt is too large ({estimated_tokens} estimated tokens > {WORKFLOW_AGENT_PROMPT_MAX_TOKENS} token cap)"
        ));
    }
    ensure_workflow_agent_prompt(prompt)?;
    Ok(())
}

/// Validate bounded string options before any value is copied into configuration, progress, or a
/// journal record.
pub(super) fn ensure_agent_options_within_bounds(
    label: Option<&str>,
    phase: Option<&str>,
    model: Option<&str>,
    effort: Option<&str>,
    agent_type: Option<&str>,
    isolation: Option<&str>,
) -> Result<(), String> {
    if let Some(label) = label {
        ensure_workflow_agent_label(label)?;
    }
    if let Some(phase) = phase {
        ensure_workflow_phase_title(phase)?;
    }
    for (field, value) in [
        ("workflow agent model", model),
        ("workflow agent effort", effort),
        ("workflow agent type", agent_type),
        ("workflow agent isolation", isolation),
    ] {
        if let Some(value) = value {
            ensure_workflow_agent_option(field, value)?;
        }
    }
    Ok(())
}

/// Reject an `agent()` `opts.schema` before it is copied into a child request or compiled.
pub(super) fn ensure_schema_within_bounds(schema: &JsonValue) -> Result<(), String> {
    codex_code_mode::ensure_workflow_agent_schema(schema)
}

#[cfg(test)]
#[path = "workflow_context_bounds_tests.rs"]
mod tests;

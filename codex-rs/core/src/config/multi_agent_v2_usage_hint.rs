use super::DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT;
use super::DEFAULT_MULTI_AGENT_V2_ROOT_AGENT_USAGE_HINT_TEXT;
use super::DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT;
use super::DEFAULT_MULTI_AGENT_V2_SUBAGENT_USAGE_HINT_TEXT;

pub(super) fn default_multi_agent_v2_usage_hint_text(
    usage_hint_text: &str,
    max_concurrency: usize,
) -> String {
    format!(
        "{usage_hint_text}\n{DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT}\nThere are {max_concurrency} available concurrency slots, meaning that up to {max_concurrency} agents can be active at once, including you."
    )
}

/// Recognize unmarked usage hints emitted before they had durable contextual-fragment identity.
/// Only built-in templates are safe to recognize after config drift; arbitrary developer messages
/// and legacy custom hints must be preserved. If the built-in templates change, retain recognition
/// for every pre-marker form that older rollouts can contain.
pub(crate) fn is_legacy_default_multi_agent_v2_usage_hint_text(text: &str) -> bool {
    [
        DEFAULT_MULTI_AGENT_V2_ROOT_AGENT_USAGE_HINT_TEXT,
        DEFAULT_MULTI_AGENT_V2_SUBAGENT_USAGE_HINT_TEXT,
    ]
    .into_iter()
    .any(|base| {
        let Some(concurrency_clause) = text
            .strip_prefix(base)
            .and_then(|text| text.strip_prefix('\n'))
            .and_then(|text| text.strip_prefix(DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT))
            .and_then(|text| text.strip_prefix("\nThere are "))
        else {
            return false;
        };
        let Some((available, active)) =
            concurrency_clause.split_once(" available concurrency slots, meaning that up to ")
        else {
            return false;
        };
        let active = active
            .strip_suffix(DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT)
            .and_then(|text| text.strip_suffix("\n\n"))
            .unwrap_or(active);
        let Some(active) = active.strip_suffix(" agents can be active at once, including you.")
        else {
            return false;
        };
        let Ok(available_count) = available.parse::<u64>() else {
            return false;
        };
        available_count != 0 && available == active && available_count.to_string() == available
    })
}

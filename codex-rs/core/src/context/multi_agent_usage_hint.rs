use super::ContextualUserFragment;
use crate::config::MULTI_AGENT_V2_PROMPT_FIELD_MAX_BYTES;

// These markers are persisted rollout identity. Keep recognizing prior markers if this format
// changes so forked and resumed histories can migrate old hints safely.
const MULTI_AGENT_USAGE_HINT_OPEN_TAG: &str = "<multi_agent_usage_hint>\n";
const MULTI_AGENT_USAGE_HINT_CLOSE_TAG: &str = "\n</multi_agent_usage_hint>";

/// A Workflow-admitted root or subagent usage hint with durable model-context identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiAgentUsageHint {
    text: String,
}

impl MultiAgentUsageHint {
    /// Defensively reapplies the Workflow prompt-field cap at the context boundary so direct
    /// in-memory config mutation cannot inject an unbounded fragment after config admission.
    /// Truncation backs up to a UTF-8 character boundary.
    pub(crate) fn new(text: &str) -> Self {
        let mut end = text.len().min(MULTI_AGENT_V2_PROMPT_FIELD_MAX_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        Self {
            text: text[..end].to_string(),
        }
    }
}

impl ContextualUserFragment for MultiAgentUsageHint {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        (
            MULTI_AGENT_USAGE_HINT_OPEN_TAG,
            MULTI_AGENT_USAGE_HINT_CLOSE_TAG,
        )
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

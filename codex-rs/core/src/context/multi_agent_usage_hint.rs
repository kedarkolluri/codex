use super::ContextualUserFragment;

const MULTI_AGENT_USAGE_HINT_OPEN_TAG: &str = "<multi_agent_usage_hint>\n";
const MULTI_AGENT_USAGE_HINT_CLOSE_TAG: &str = "\n</multi_agent_usage_hint>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MultiAgentUsageHint {
    text: String,
}

impl MultiAgentUsageHint {
    pub(crate) fn new(text: &str) -> Self {
        Self {
            text: text.to_string(),
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

use crate::config::is_legacy_default_multi_agent_v2_usage_hint_text;
use crate::context::ContextualUserFragment;
use crate::context::MultiAgentModeInstructions;
use crate::context::MultiAgentUsageHint;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

pub(super) fn sanitize_message(item: &mut ResponseItem, usage_hint_texts: &[String]) -> bool {
    let ResponseItem::Message { role, content, .. } = item else {
        return true;
    };
    if role != "developer" {
        return true;
    }
    let standalone_content = content.len() == 1;
    content.retain(|content_item| {
        let ContentItem::InputText { text } = content_item else {
            return true;
        };
        if MultiAgentModeInstructions::matches_text(text) {
            return true;
        }
        !(MultiAgentUsageHint::matches_text(text)
            || is_legacy_default_multi_agent_v2_usage_hint_text(text)
            || standalone_content
                && usage_hint_texts
                    .iter()
                    .any(|usage_hint_text| usage_hint_text == text))
    });
    !content.is_empty()
}

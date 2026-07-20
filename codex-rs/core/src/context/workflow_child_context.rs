use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;

use crate::config::is_legacy_default_multi_agent_v2_usage_hint_text;
use super::ContextualUserFragment;
use super::MultiAgentModeInstructions;
use super::MultiAgentUsageHint;
use super::workflow_child_tools::bound_workflow_child_output_items;

/// Every text fragment injected into a workflow-managed child is emitted as its own item and is
/// bounded by bytes. A byte limit below 10K is a tokenizer-independent upper bound on the number
/// of non-empty tokens that the fragment can produce.
pub(crate) const MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES: usize = 8 * 1024;
/// A single context-building or injection boundary cannot add an unbounded number of otherwise
/// valid fragments to a workflow-managed child.
pub(crate) const MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS: usize = 64;
/// Serialized size backstop for one context-building or injection boundary. This is deliberately
/// independent of the item-count ceiling because many individually safe items can still create an
/// unsafe aggregate prompt.
pub(crate) const MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES: usize = 64 * 1024;

const TRUNCATION_MARKER: &str = "\n... [workflow context truncated] ...\n";
const BATCH_OMISSION_MARKER: &str = "... [additional workflow context omitted]";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowChildDeveloperContext {
    text: String,
}

impl WorkflowChildDeveloperContext {
    fn new(text: String) -> Self {
        Self {
            text: bound_text(text),
        }
    }
}

impl ContextualUserFragment for WorkflowChildDeveloperContext {
    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowChildUserContext {
    text: String,
}

impl WorkflowChildUserContext {
    fn new(text: String) -> Self {
        Self {
            text: bound_text(text),
        }
    }
}

impl ContextualUserFragment for WorkflowChildUserContext {
    fn role(&self) -> &'static str {
        "user"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        Self::type_markers()
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("", "")
    }

    fn body(&self) -> String {
        self.text.clone()
    }
}

/// Applies the workflow-child context boundary to already rendered injected fragments.
///
/// Context builders historically aggregate multiple text fragments into one message. Workflow
/// children instead receive one bounded message per text fragment. Unknown non-developer roles
/// are lowered to `user`, unsupported non-text context is discarded, and stale collaboration
/// guidance is omitted because workflow children do not expose collaboration tools.
pub(crate) fn bound_workflow_child_context_items(
    items: Vec<ResponseItem>,
) -> Vec<ResponseItem> {
    bound_context_batch(items)
}

/// Bounds message-shaped context entering through the generic injection API while preserving
/// non-message tool and protocol items that are governed by their own output limits.
pub(crate) fn bound_workflow_child_injected_messages(
    items: Vec<ResponseItem>,
) -> Vec<ResponseItem> {
    let mut messages = Vec::new();
    let mut outputs = Vec::new();
    for item in items {
        match item {
            ResponseItem::Message { .. } => messages.push(item),
            ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. } => outputs.push(item),
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::ContextCompaction { .. }
            | ResponseItem::Other => {}
        }
    }
    let mut bounded = bound_context_batch(messages);
    bounded.extend(bound_workflow_child_output_items(outputs));
    bounded
}

/// Validates the intentionally narrow app-server injection surface for workflow-managed children.
/// Internal tool outputs use a separate typed path and are bounded at the history boundary.
pub(crate) fn validate_workflow_child_client_injected_items(
    items: &[ResponseItem],
) -> Result<(), &'static str> {
    let mut fragment_count = 0usize;
    for item in items {
        let ResponseItem::Message { content, .. } = item else {
            return Err("workflow-managed threads accept only text message injection");
        };
        for content_item in content {
            if !matches!(
                content_item,
                ContentItem::InputText { .. } | ContentItem::OutputText { .. }
            ) {
                return Err("workflow-managed threads accept only text message injection");
            }
            fragment_count = fragment_count.saturating_add(1);
            if fragment_count > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS {
                return Err("workflow-managed thread injection exceeds the 64-item limit");
            }
        }
    }
    Ok(())
}

fn bound_context_batch(items: Vec<ResponseItem>) -> Vec<ResponseItem> {
    let mut bounded = Vec::with_capacity(items.len().min(MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS));
    let mut serialized_bytes = 0usize;
    let mut omitted = false;

    'outer: for item in items {
        for item in bound_workflow_child_context_item(item) {
            let item_bytes = serialized_len(&item);
            if bounded.len() == MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS
                || serialized_bytes.saturating_add(item_bytes)
                    > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
            {
                omitted = true;
                break 'outer;
            }
            serialized_bytes += item_bytes;
            bounded.push(item);
        }
    }

    if omitted {
        let marker: ResponseItem =
            ContextualUserFragment::into(WorkflowChildUserContext::new(
                BATCH_OMISSION_MARKER.to_string(),
            ));
        let marker_bytes = serialized_len(&marker);
        while bounded.len() == MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS
            || serialized_bytes.saturating_add(marker_bytes)
                > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
        {
            let Some(removed) = bounded.pop() else {
                break;
            };
            serialized_bytes = serialized_bytes.saturating_sub(serialized_len(&removed));
        }
        bounded.push(marker);
    }

    bounded
}

fn serialized_len(item: &ResponseItem) -> usize {
    serde_json::to_vec(item).map_or(usize::MAX, |serialized| serialized.len())
}

fn bound_workflow_child_context_item(item: ResponseItem) -> Vec<ResponseItem> {
    let ResponseItem::Message {
        role,
        content,
        phase,
        ..
    } = item
    else {
        return Vec::new();
    };

    let developer_role = role == "developer";
    content
        .into_iter()
        .filter_map(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                if is_stale_multi_agent_context(&text) {
                    return None;
                }
                let mut item = if developer_role {
                    ContextualUserFragment::into(WorkflowChildDeveloperContext::new(text))
                } else {
                    ContextualUserFragment::into(WorkflowChildUserContext::new(text))
                };
                if let ResponseItem::Message {
                    phase: bounded_phase,
                    ..
                } = &mut item
                {
                    *bounded_phase = phase.clone();
                }
                Some(bound_workflow_child_text_message(item))
            }
            ContentItem::InputImage { .. } => None,
        })
        .collect()
}

/// Clears untrusted message-envelope fields and bounds the complete serialized message item.
/// Callers provide a single text content item; workflow context splitting happens before this
/// boundary.
pub(crate) fn bound_workflow_child_text_message(item: ResponseItem) -> ResponseItem {
    let ResponseItem::Message {
        role,
        content,
        phase,
        ..
    } = item
    else {
        return item;
    };
    let [content_item] = content.as_slice() else {
        return ResponseItem::Message {
            id: None,
            role,
            content: Vec::new(),
            phase,
            internal_chat_message_metadata_passthrough: None,
        };
    };
    let (text, output) = match content_item {
        ContentItem::InputText { text } => (text.clone(), false),
        ContentItem::OutputText { text } => (text.clone(), true),
        ContentItem::InputImage { .. } => {
            return ResponseItem::Message {
                id: None,
                role,
                content: Vec::new(),
                phase,
                internal_chat_message_metadata_passthrough: None,
            };
        }
    };
    let build = |text: String| ResponseItem::Message {
        id: None,
        role: role.clone(),
        content: vec![if output {
            ContentItem::OutputText { text }
        } else {
            ContentItem::InputText { text }
        }],
        phase: phase.clone(),
        internal_chat_message_metadata_passthrough: None,
    };
    let bounded = bound_text(text.clone());
    let item = build(bounded.clone());
    if serialized_len(&item) <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
        return item;
    }

    let mut retained_bytes = bounded.len().saturating_sub(1);
    loop {
        let candidate = truncate_text_with_marker(&text, retained_bytes);
        let item = build(candidate);
        if serialized_len(&item) <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
            return item;
        }
        if retained_bytes == 0 {
            return build(String::new());
        }
        retained_bytes /= 2;
    }
}

pub(crate) fn is_stale_multi_agent_context(text: &str) -> bool {
    MultiAgentModeInstructions::matches_text(text)
        || MultiAgentUsageHint::matches_text(text)
        || is_legacy_default_multi_agent_v2_usage_hint_text(text)
}

pub(crate) fn bound_text(text: String) -> String {
    if text.len() <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
        return text;
    }

    let retained_bytes = MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES - TRUNCATION_MARKER.len();
    let prefix_budget = retained_bytes / 2;
    let suffix_budget = retained_bytes - prefix_budget;
    let prefix_end = floor_char_boundary(&text, prefix_budget);
    let suffix_start = ceil_char_boundary(
        &text,
        text.len().saturating_sub(suffix_budget).max(prefix_end),
    );

    let mut bounded = String::with_capacity(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    bounded.push_str(&text[..prefix_end]);
    bounded.push_str(TRUNCATION_MARKER);
    bounded.push_str(&text[suffix_start..]);
    debug_assert!(bounded.len() <= MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES);
    bounded
}

fn truncate_text_with_marker(text: &str, retained_bytes: usize) -> String {
    let retained_bytes = retained_bytes.min(text.len());
    let prefix_budget = retained_bytes / 2;
    let suffix_budget = retained_bytes - prefix_budget;
    let prefix_end = floor_char_boundary(text, prefix_budget);
    let suffix_start = ceil_char_boundary(
        text,
        text.len().saturating_sub(suffix_budget).max(prefix_end),
    );
    let mut bounded = String::with_capacity(retained_bytes + TRUNCATION_MARKER.len());
    bounded.push_str(&text[..prefix_end]);
    bounded.push_str(TRUNCATION_MARKER);
    bounded.push_str(&text[suffix_start..]);
    bounded
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

#[cfg(test)]
#[path = "workflow_child_context_tests.rs"]
mod tests;

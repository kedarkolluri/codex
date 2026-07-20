//! Validation and compaction bounds for workflow-managed child model history.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;

use crate::context_manager::is_model_generated_item;
use super::workflow_child_context::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES;
use super::workflow_child_context::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS;
use super::workflow_child_context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES;
use super::workflow_child_context::bound_text;
use super::workflow_child_context::bound_workflow_child_text_message;
use super::workflow_child_context::is_stale_multi_agent_context;
use super::workflow_child_tools::bound_workflow_child_output_item;

const HISTORY_OMISSION_MARKER: &str = "... [additional workflow history omitted]";

/// Validates the complete model input immediately before sampling or installing a resumed
/// history. This is a fail-closed guard; callers must not replace the stored history with a
/// sanitized derivative.
pub(crate) fn validate_workflow_child_model_history(
    items: &[ResponseItem],
) -> Result<(), &'static str> {
    let mut logical_items = 0usize;
    let mut aggregate_bytes = 0usize;
    for item in items {
        if let ResponseItem::Message { role, content, .. } = item
            && role == "assistant"
        {
            if InterAgentCommunication::is_message_content(content) {
                return Err("workflow child history contains inter-agent communication");
            }
            if content
                .iter()
                .any(|content_item| matches!(content_item, ContentItem::InputImage { .. }))
            {
                return Err("workflow child history contains image context");
            }
        }
        if is_model_generated_item(item) {
            logical_items = 0;
            aggregate_bytes = 0;
            continue;
        }
        match item {
            ResponseItem::AgentMessage { .. } => {
                return Err("workflow child history contains inter-agent communication");
            }
            item @ (ResponseItem::AdditionalTools { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }) => {
                let serialized_bytes = serde_json::to_vec(item)
                    .map_err(|_| "workflow child history item cannot be serialized")?
                    .len();
                if serialized_bytes > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
                    || bound_workflow_child_output_item(item.clone()) != *item
                {
                    return Err("workflow child history contains an oversized tool output");
                }
            }
            ResponseItem::Message { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::Other => {}
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. } => {
                unreachable!("model-generated items are handled before injected segments")
            }
        }

        let (item_count, item_bytes) = model_visible_item_usage(item)?;
        logical_items = logical_items.saturating_add(item_count);
        aggregate_bytes = aggregate_bytes.saturating_add(item_bytes);
        if logical_items > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS
            || aggregate_bytes > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
        {
            return Err("workflow child history exceeds model-context limits");
        }
    }
    Ok(())
}

/// Bounds a newly produced compaction replacement before it can advance or replace the live
/// history window. Only message and semantic compaction items are valid compacted output.
pub(crate) fn bound_workflow_child_compacted_history(
    items: Vec<ResponseItem>,
) -> Result<Vec<ResponseItem>, &'static str> {
    let mut candidates = Vec::new();
    for item in items {
        match item {
            ResponseItem::Message { .. } => {
                candidates.extend(bound_compacted_message(item));
            }
            item @ (ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }) => {
                let (_, bytes) = model_visible_item_usage(&item)?;
                if bytes > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
                    return Err("workflow child compaction item exceeds model-context limits");
                }
                candidates.push(item);
            }
            ResponseItem::AdditionalTools { .. }
            | ResponseItem::AgentMessage { .. }
            | ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::CompactionTrigger { .. }
            | ResponseItem::Other => {
                return Err("workflow child compaction contains an unsupported item");
            }
        }
    }

    let trailing_compaction = candidates
        .last()
        .is_some_and(|item| matches!(item, ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }));
    if candidates
        .iter()
        .take(candidates.len().saturating_sub(usize::from(trailing_compaction)))
        .any(|item| matches!(item, ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }))
    {
        return Err("workflow child compaction contains misplaced semantic items");
    }
    let semantic_tail = trailing_compaction.then(|| candidates.pop()).flatten();
    let semantic_tail_bytes = semantic_tail
        .as_ref()
        .map(model_visible_item_usage)
        .transpose()?
        .map_or(0, |(_, bytes)| bytes);
    let reserved_items = usize::from(semantic_tail.is_some());

    let mut bounded = Vec::new();
    let mut aggregate_bytes = 0usize;
    let mut omitted = false;
    for item in candidates {
        let (item_count, item_bytes) = model_visible_item_usage(&item)?;
        if bounded.len().saturating_add(item_count).saturating_add(reserved_items)
            > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS
            || aggregate_bytes
                .saturating_add(item_bytes)
                .saturating_add(semantic_tail_bytes)
                > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
        {
            omitted = true;
            break;
        }
        aggregate_bytes += item_bytes;
        bounded.push(item);
    }

    if omitted {
        let marker = history_omission_item();
        let (_, marker_bytes) = model_visible_item_usage(&marker)?;
        while bounded.len().saturating_add(1).saturating_add(reserved_items)
            > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS
            || aggregate_bytes
                .saturating_add(marker_bytes)
                .saturating_add(semantic_tail_bytes)
                > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
        {
            let Some(removed) = bounded.pop() else {
                break;
            };
            aggregate_bytes = aggregate_bytes
                .saturating_sub(model_visible_item_usage(&removed)?.1);
        }
        bounded.push(marker);
    }
    if let Some(semantic_tail) = semantic_tail {
        bounded.push(semantic_tail);
    }
    validate_workflow_child_model_history(&bounded)?;
    Ok(bounded)
}

fn bound_compacted_message(item: ResponseItem) -> Vec<ResponseItem> {
    let ResponseItem::Message {
        role,
        content,
        phase,
        ..
    } = item
    else {
        return Vec::new();
    };
    content
        .into_iter()
        .filter_map(|content_item| {
            let (text, output) = match content_item {
                ContentItem::InputText { text } => (text, false),
                ContentItem::OutputText { text } => (text, true),
                ContentItem::InputImage { .. } => return None,
            };
            if role != "assistant" && is_stale_multi_agent_context(&text) {
                return None;
            }
            let text = bound_text(text);
            Some(bound_workflow_child_text_message(ResponseItem::Message {
                id: None,
                role: role.clone(),
                content: vec![if output {
                    ContentItem::OutputText { text }
                } else {
                    ContentItem::InputText { text }
                }],
                phase: phase.clone(),
                internal_chat_message_metadata_passthrough: None,
            }))
        })
        .collect()
}

fn model_visible_item_usage(item: &ResponseItem) -> Result<(usize, usize), &'static str> {
    match item {
        ResponseItem::Message { content, .. } => {
            let mut bytes = 0usize;
            for content_item in content {
                let text = match content_item {
                    ContentItem::InputText { text } | ContentItem::OutputText { text } => text,
                    ContentItem::InputImage { .. } => {
                        return Err("workflow child history contains image context");
                    }
                };
                if text.len() > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
                    return Err("workflow child history contains an oversized message");
                }
                bytes = bytes.saturating_add(text.len());
            }
            let serialized_bytes = serde_json::to_vec(item)
                .map_err(|_| "workflow child history item cannot be serialized")?
                .len();
            if serialized_bytes > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
                return Err("workflow child history contains an oversized message");
            }
            Ok((content.len().max(1), bytes))
        }
        item => {
            let bytes = serde_json::to_vec(item)
                .map_err(|_| "workflow child history item cannot be serialized")?
                .len();
            if bytes > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
                return Err("workflow child history contains an oversized protocol item");
            }
            Ok((1, bytes))
        }
    }
}

fn history_omission_item() -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: HISTORY_OMISSION_MARKER.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[cfg(test)]
#[path = "workflow_child_history_tests.rs"]
mod tests;

//! Validation and compaction bounds for workflow-managed child model history.

use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::InterAgentCommunication;

use super::workflow_child_context::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES;
use super::workflow_child_context::MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS;
use super::workflow_child_context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES;
use super::workflow_child_context::bound_text;
use super::workflow_child_context::bound_workflow_child_text_message;
use super::workflow_child_context::is_stale_multi_agent_context;
use super::workflow_child_tools::bound_workflow_child_output_item;
use super::workflow_child_tools::omitted_tool_output;
use crate::context_manager::is_model_generated_item;
use crate::context_manager::is_user_turn_boundary;
use codex_utils_string::take_bytes_at_char_boundary;

const HISTORY_OMISSION_MARKER: &str = "... [additional workflow history omitted]";
const FINALIZED_CONTEXT_TRUNCATION_MARKER: &str =
    "\n... [workflow finalized context truncated] ...\n";
pub(crate) const MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES: usize =
    MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES - MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES;
const MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_ITEMS: usize = MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS - 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkflowChildHistoryAppendMode {
    Noncritical,
    ExactUserPrompt,
}

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

/// Finalizes one append against the already-recorded trailing injected segment.
///
/// Existing history is inspected but never rewritten. Model-generated items reset segment
/// accounting and are retained byte-for-byte. Before the first real user message in a new turn,
/// one complete item slot is reserved so a prevalidated workflow `agent()` prompt can be retained
/// exactly after its UUID ID and turn metadata are assigned.
pub(crate) fn finalize_workflow_child_history_append(
    existing_history: &[ResponseItem],
    items: Vec<ResponseItem>,
    mode: WorkflowChildHistoryAppendMode,
) -> Result<Vec<ResponseItem>, &'static str> {
    let mut usage = trailing_injected_segment_usage(existing_history)?;
    let mut finalized = Vec::with_capacity(items.len());
    for item in items {
        if is_model_generated_item(&item) {
            usage.reset_after_model_item(&item);
            finalized.push(item);
            continue;
        }

        let user_turn_boundary = is_user_turn_boundary(&item);
        if mode == WorkflowChildHistoryAppendMode::ExactUserPrompt {
            if !user_turn_boundary {
                return Err("workflow child exact prompt append is not a user message");
            }
            let (item_count, item_bytes) = model_visible_item_usage(&item)?;
            if !usage.can_add(item_count, item_bytes, /*adding_user_prompt*/ true) {
                return Err("workflow child prompt exceeds finalized model-context limits");
            }
            usage.add(item_count, item_bytes, /*added_user_prompt*/ true);
            finalized.push(item);
            continue;
        }

        // Role is not an authority boundary: externally injected messages can claim `user`.
        // Only the dedicated ExactUserPrompt path may consume the reserved supervisor slot.
        let (item_limit, byte_limit) = usage.limits(/*adding_user_prompt*/ false);
        let remaining_items = item_limit.saturating_sub(usage.logical_items);
        let remaining_bytes = byte_limit.saturating_sub(usage.serialized_bytes);
        let candidate = match item {
            item @ ResponseItem::Message { .. } => bound_finalized_noncritical_message(
                item,
                remaining_items,
                remaining_bytes.min(MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES),
            ),
            ResponseItem::AgentMessage { .. } => None,
            item @ (ResponseItem::AdditionalTools { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }) => {
                let bounded = bound_workflow_child_output_item(item);
                if finalized_item_fits(&usage, &bounded, /*adding_user_prompt*/ false) {
                    Some(bounded)
                } else {
                    let omitted = omitted_tool_output(bounded);
                    finalized_item_fits(&usage, &omitted, /*adding_user_prompt*/ false)
                        .then_some(omitted)
                }
            }
            item @ (ResponseItem::CompactionTrigger { .. } | ResponseItem::Other) => {
                finalized_item_fits(&usage, &item, /*adding_user_prompt*/ false).then_some(item)
            }
            ResponseItem::Reasoning { .. }
            | ResponseItem::LocalShellCall { .. }
            | ResponseItem::FunctionCall { .. }
            | ResponseItem::ToolSearchCall { .. }
            | ResponseItem::CustomToolCall { .. }
            | ResponseItem::WebSearchCall { .. }
            | ResponseItem::ImageGenerationCall { .. }
            | ResponseItem::Compaction { .. }
            | ResponseItem::ContextCompaction { .. } => {
                unreachable!("model-generated items are handled before finalized appends")
            }
        };
        let Some(candidate) = candidate else {
            continue;
        };
        let (item_count, item_bytes) = model_visible_item_usage(&candidate)?;
        if !usage.can_add(item_count, item_bytes, /*adding_user_prompt*/ false) {
            continue;
        }
        usage.add(item_count, item_bytes, /*added_user_prompt*/ false);
        finalized.push(candidate);
    }
    Ok(finalized)
}

#[derive(Debug, Default)]
struct InjectedSegmentUsage {
    logical_items: usize,
    serialized_bytes: usize,
    reserve_user_prompt: bool,
}

impl InjectedSegmentUsage {
    fn limits(&self, adding_user_prompt: bool) -> (usize, usize) {
        if self.reserve_user_prompt && !adding_user_prompt {
            (
                MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_ITEMS,
                MAX_WORKFLOW_CHILD_PRE_USER_CONTEXT_BYTES,
            )
        } else {
            (
                MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS,
                MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES,
            )
        }
    }

    fn can_add(&self, item_count: usize, item_bytes: usize, adding_user_prompt: bool) -> bool {
        let (item_limit, byte_limit) = self.limits(adding_user_prompt);
        self.logical_items.saturating_add(item_count) <= item_limit
            && self.serialized_bytes.saturating_add(item_bytes) <= byte_limit
    }

    fn add(&mut self, item_count: usize, item_bytes: usize, added_user_prompt: bool) {
        self.logical_items = self.logical_items.saturating_add(item_count);
        self.serialized_bytes = self.serialized_bytes.saturating_add(item_bytes);
        if added_user_prompt {
            self.reserve_user_prompt = false;
        }
    }

    fn reset_after_model_item(&mut self, item: &ResponseItem) {
        self.logical_items = 0;
        self.serialized_bytes = 0;
        self.reserve_user_prompt = reserves_user_prompt_after(item);
    }
}

fn reserves_user_prompt_after(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::Message { role, .. } if role == "assistant"
    ) || matches!(
        item,
        ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
    )
}

fn trailing_injected_segment_usage(
    existing_history: &[ResponseItem],
) -> Result<InjectedSegmentUsage, &'static str> {
    let last_model_item = existing_history.iter().rposition(is_model_generated_item);
    let mut usage = InjectedSegmentUsage {
        reserve_user_prompt: last_model_item
            .is_none_or(|index| reserves_user_prompt_after(&existing_history[index])),
        ..InjectedSegmentUsage::default()
    };
    let suffix_start = last_model_item.map_or(0, |index| index + 1);
    for item in &existing_history[suffix_start..] {
        let (item_count, item_bytes) = model_visible_item_usage(item)?;
        // Persisted role/content alone cannot prove that a user-shaped item came through the
        // trusted supervisor prompt path, so retain the reservation conservatively.
        usage.add(item_count, item_bytes, /*added_user_prompt*/ false);
    }
    Ok(usage)
}

fn finalized_item_fits(
    usage: &InjectedSegmentUsage,
    item: &ResponseItem,
    adding_user_prompt: bool,
) -> bool {
    model_visible_item_usage(item).is_ok_and(|(item_count, item_bytes)| {
        usage.can_add(item_count, item_bytes, adding_user_prompt)
    })
}

fn bound_finalized_noncritical_message(
    mut item: ResponseItem,
    max_logical_items: usize,
    max_serialized_bytes: usize,
) -> Option<ResponseItem> {
    {
        let ResponseItem::Message { content, .. } = &mut item else {
            return None;
        };
        content.retain(|content_item| {
            matches!(
                content_item,
                ContentItem::InputText { .. } | ContentItem::OutputText { .. }
            )
        });
        content.truncate(max_logical_items);
        if content.is_empty() || max_serialized_bytes == 0 {
            return None;
        }
    }
    loop {
        let content_len = match &item {
            ResponseItem::Message { content, .. } => content.len(),
            _ => return None,
        };
        if content_len <= 1 || serialized_item_bytes(&item) <= max_serialized_bytes {
            break;
        }
        let ResponseItem::Message { content, .. } = &mut item else {
            return None;
        };
        content.pop();
    }
    if serialized_item_bytes(&item) <= max_serialized_bytes {
        return Some(item);
    }

    let original_text = match &item {
        ResponseItem::Message { content, .. } => match &content[0] {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => text.clone(),
            ContentItem::InputImage { .. } => return None,
        },
        _ => return None,
    };
    let set_text = |item: &mut ResponseItem, text: String| {
        let ResponseItem::Message { content, .. } = item else {
            return;
        };
        match &mut content[0] {
            ContentItem::InputText { text: current_text }
            | ContentItem::OutputText { text: current_text } => *current_text = text,
            ContentItem::InputImage { .. } => {}
        }
    };
    set_text(&mut item, FINALIZED_CONTEXT_TRUNCATION_MARKER.to_string());
    if serialized_item_bytes(&item) > max_serialized_bytes {
        return None;
    }
    let mut best = FINALIZED_CONTEXT_TRUNCATION_MARKER.to_string();
    let mut low = 0usize;
    let mut high = original_text.len();
    while low <= high {
        let retained_bytes = low + (high - low) / 2;
        let prefix = take_bytes_at_char_boundary(&original_text, retained_bytes);
        let candidate = format!("{prefix}{FINALIZED_CONTEXT_TRUNCATION_MARKER}");
        set_text(&mut item, candidate.clone());
        if serialized_item_bytes(&item) <= max_serialized_bytes {
            best = candidate;
            low = retained_bytes.saturating_add(1);
        } else if retained_bytes == 0 {
            break;
        } else {
            high = retained_bytes - 1;
        }
    }
    set_text(&mut item, best);
    Some(item)
}

fn serialized_item_bytes(item: &ResponseItem) -> usize {
    serde_json::to_vec(item).map_or(usize::MAX, |serialized| serialized.len())
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

    let trailing_compaction = candidates.last().is_some_and(|item| {
        matches!(
            item,
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
        )
    });
    if candidates
        .iter()
        .take(
            candidates
                .len()
                .saturating_sub(usize::from(trailing_compaction)),
        )
        .any(|item| {
            matches!(
                item,
                ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. }
            )
        })
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
        if bounded
            .len()
            .saturating_add(item_count)
            .saturating_add(reserved_items)
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
        while bounded
            .len()
            .saturating_add(1)
            .saturating_add(reserved_items)
            > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS
            || aggregate_bytes
                .saturating_add(marker_bytes)
                .saturating_add(semantic_tail_bytes)
                > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
        {
            let Some(removed) = bounded.pop() else {
                break;
            };
            aggregate_bytes = aggregate_bytes.saturating_sub(model_visible_item_usage(&removed)?.1);
        }
        bounded.push(marker);
    }
    if let Some(semantic_tail) = semantic_tail {
        bounded.push(semantic_tail);
    }
    validate_workflow_child_compacted_history(&bounded)?;
    Ok(bounded)
}

/// Validates the exact, final serialized form of a workflow compaction replacement.
///
/// Unlike general active-history validation, every compaction item is treated as newly injected
/// context even when it has an assistant role or a model-generated protocol shape. Callers must
/// run this after assigning response-item IDs because those IDs contribute to the per-item cap.
pub(crate) fn validate_workflow_child_compacted_history(
    items: &[ResponseItem],
) -> Result<(), &'static str> {
    let mut logical_items = 0usize;
    let mut aggregate_bytes = 0usize;
    for (index, item) in items.iter().enumerate() {
        match item {
            ResponseItem::Message { .. } => {}
            ResponseItem::Compaction { .. } | ResponseItem::ContextCompaction { .. } => {
                if index + 1 != items.len() {
                    return Err("workflow child compaction contains misplaced semantic items");
                }
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
        let (item_count, item_bytes) = model_visible_item_usage(item)?;
        logical_items = logical_items.saturating_add(item_count);
        aggregate_bytes = aggregate_bytes.saturating_add(item_bytes);
        if logical_items > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_ITEMS
            || aggregate_bytes > MAX_WORKFLOW_CHILD_CONTEXT_BATCH_BYTES
        {
            return Err("workflow child history exceeds model-context limits");
        }
    }
    validate_workflow_child_model_history(items)
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
            }
            let serialized_bytes = serde_json::to_vec(item)
                .map_err(|_| "workflow child history item cannot be serialized")?
                .len();
            if serialized_bytes > MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES {
                return Err("workflow child history contains an oversized message");
            }
            Ok((content.len().max(1), serialized_bytes))
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

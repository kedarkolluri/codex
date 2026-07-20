//! Hard bounds for tool definitions and tool outputs exposed to workflow-managed children.

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ToolSpec;
use codex_tools::create_tools_json_for_responses_api;
use serde::Serialize;

use crate::config::Config;

/// Maximum serialized bytes for one logical tool definition.
pub(crate) const MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES: usize = 8 * 1024;
/// Maximum logical tools exposed in one workflow-child prompt.
pub(crate) const MAX_WORKFLOW_CHILD_TOOL_SPECS: usize = 64;
/// Maximum serialized bytes for the complete workflow-child tool array.
pub(crate) const MAX_WORKFLOW_CHILD_TOOL_SPECS_BYTES: usize = 64 * 1024;
/// Maximum serialized bytes for one function/custom/tool-search output payload.
pub(crate) const MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES: usize = 8 * 1024;
/// Maximum serialized bytes for one structured tool-output content item.
pub(crate) const MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES: usize = 4 * 1024;
/// Maximum structured content items or discovered tools in one tool output.
pub(crate) const MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS: usize = 16;
/// Maximum tool-output response items accepted from one recording/injection batch.
pub(crate) const MAX_WORKFLOW_CHILD_OUTPUT_ITEMS: usize = 64;
/// Maximum serialized bytes for all tool-output response items retained from one batch.
pub(crate) const MAX_WORKFLOW_CHILD_OUTPUT_BATCH_BYTES: usize = 64 * 1024;
/// Repository-wide maximum number of tokens allowed for one tool output.
pub(crate) const MAX_WORKFLOW_CHILD_TOOL_OUTPUT_TOKENS: usize = 10_000;

const OUTPUT_TRUNCATION_MARKER: &str = "\n... [workflow tool output truncated] ...\n";

/// Drops unsafe or excess tool definitions while preserving the order of retained tools.
///
/// Namespace wrappers are retained as a single API tool, but each nested function counts as one
/// logical definition and must independently satisfy the per-spec limit. Schemas and
/// descriptions are never partially rewritten.
pub(crate) fn bound_workflow_child_tool_specs(tools: Vec<ToolSpec>) -> Vec<ToolSpec> {
    bound_workflow_child_tool_specs_with_aggregate(
        tools,
        MAX_WORKFLOW_CHILD_TOOL_SPECS_BYTES,
    )
}

/// Responses Lite embeds the complete tool array in one `AdditionalTools` input item. Apply the
/// per-output-payload ceiling to that array in addition to the normal per-spec and count bounds.
pub(crate) fn bound_workflow_child_responses_lite_tool_specs(
    tools: Vec<ToolSpec>,
) -> Vec<ToolSpec> {
    let mut bounded = bound_workflow_child_tool_specs_with_aggregate(
        tools,
        MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES,
    );
    while !responses_lite_additional_tools_fits(&bounded) {
        let remove_wrapper = match bounded.last_mut() {
            Some(ToolSpec::Namespace(namespace)) if namespace.tools.len() > 1 => {
                namespace.tools.pop();
                false
            }
            Some(_) => true,
            None => break,
        };
        if remove_wrapper {
            bounded.pop();
        }
    }
    bounded
}

fn bound_workflow_child_tool_specs_with_aggregate(
    tools: Vec<ToolSpec>,
    aggregate_bytes: usize,
) -> Vec<ToolSpec> {
    let mut bounded = Vec::new();
    let mut logical_count = 0usize;

    for tool in tools {
        if logical_count >= MAX_WORKFLOW_CHILD_TOOL_SPECS {
            break;
        }
        match tool {
            ToolSpec::Namespace(namespace) => {
                let ResponsesApiNamespace {
                    name,
                    description,
                    tools,
                } = namespace;
                let namespace_shell = ToolSpec::Namespace(ResponsesApiNamespace {
                    name: name.clone(),
                    description: description.clone(),
                    tools: Vec::new(),
                });
                if serialized_len(&namespace_shell)
                    .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)
                {
                    continue;
                }

                let mut bounded_namespace_tools = Vec::new();
                for namespace_tool in tools {
                    if logical_count >= MAX_WORKFLOW_CHILD_TOOL_SPECS {
                        break;
                    }
                    if serialized_len(&namespace_tool)
                        .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)
                    {
                        continue;
                    }
                    let mut candidate_namespace_tools = bounded_namespace_tools.clone();
                    candidate_namespace_tools.push(namespace_tool.clone());
                    let candidate = ToolSpec::Namespace(ResponsesApiNamespace {
                        name: name.clone(),
                        description: description.clone(),
                        tools: candidate_namespace_tools,
                    });
                    if !tool_array_with_candidate_fits(
                        &bounded,
                        &candidate,
                        aggregate_bytes,
                    ) {
                        continue;
                    }
                    bounded_namespace_tools.push(namespace_tool);
                    logical_count += 1;
                }
                if !bounded_namespace_tools.is_empty() {
                    bounded.push(ToolSpec::Namespace(ResponsesApiNamespace {
                        name,
                        description,
                        tools: bounded_namespace_tools,
                    }));
                }
            }
            tool => {
                if serialized_len(&tool)
                    .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)
                    || !tool_array_with_candidate_fits(&bounded, &tool, aggregate_bytes)
                {
                    continue;
                }
                bounded.push(tool);
                logical_count += 1;
            }
        }
    }

    bounded
}

/// Applies output-specific bounds to a batch entering workflow-child model history.
pub(crate) fn bound_workflow_child_output_items(
    items: impl IntoIterator<Item = ResponseItem>,
) -> Vec<ResponseItem> {
    let mut bounded = Vec::new();
    let mut output_items = Vec::new();
    let mut output_positions = Vec::new();

    for item in items {
        if !is_tool_output_item(&item) {
            bounded.push(item);
            continue;
        }
        if output_items.len() >= MAX_WORKFLOW_CHILD_OUTPUT_ITEMS {
            let evicted_output = output_items.remove(0);
            let evicted_position = output_positions.remove(0);
            remove_bounded_item_at(&mut bounded, &mut output_positions, evicted_position);
            remove_matching_call_from_batch(
                &evicted_output,
                &mut bounded,
                &mut output_positions,
            );
        }

        let item = bound_workflow_child_output_item(item);
        if serialized_len(&item)
            .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES)
        {
            remove_matching_call_from_batch(&item, &mut bounded, &mut output_positions);
            continue;
        }
        let mut candidate_outputs = output_items.clone();
        candidate_outputs.push(item.clone());
        if serialized_len(&candidate_outputs)
            .is_some_and(|bytes| bytes <= MAX_WORKFLOW_CHILD_OUTPUT_BATCH_BYTES)
        {
            output_positions.push(bounded.len());
            output_items.push(item.clone());
            bounded.push(item);
            continue;
        }

        let omission = omitted_tool_output(item);
        candidate_outputs.pop();
        candidate_outputs.push(omission.clone());
        while serialized_len(&candidate_outputs)
            .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_OUTPUT_BATCH_BYTES)
        {
            let Some(previous_index) = output_items
                .iter()
                .rposition(|item| !is_omitted_tool_output(item))
            else {
                break;
            };
            let previous_omission = omitted_tool_output(output_items[previous_index].clone());
            output_items[previous_index] = previous_omission.clone();
            bounded[output_positions[previous_index]] = previous_omission.clone();
            candidate_outputs[previous_index] = previous_omission;
        }
        if serialized_len(&candidate_outputs)
            .is_some_and(|bytes| bytes <= MAX_WORKFLOW_CHILD_OUTPUT_BATCH_BYTES)
        {
            output_positions.push(bounded.len());
            output_items.push(omission.clone());
            bounded.push(omission);
        } else {
            remove_matching_call_from_batch(&omission, &mut bounded, &mut output_positions);
        }
    }

    bounded
}

/// Applies output-specific bounds while preserving model-generated call and reasoning items.
pub(crate) fn bound_workflow_child_output_item(item: ResponseItem) -> ResponseItem {
    let bounded = match item {
        ResponseItem::AdditionalTools { id, role, tools } => {
            let empty_tools = Vec::new();
            let empty_item = ResponseItem::AdditionalTools {
                id: id.clone(),
                role: role.clone(),
                tools: empty_tools.clone(),
            };
            let tools_budget = embedded_payload_budget(&empty_item, &empty_tools);
            ResponseItem::AdditionalTools {
                id,
                role,
                tools: bound_tool_values(tools, tools_budget),
            }
        }
        ResponseItem::FunctionCallOutput {
            id,
            call_id,
            output,
            internal_chat_message_metadata_passthrough,
        } => {
            let empty_output = FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(String::new()),
                success: output.success,
            };
            let empty_item = ResponseItem::FunctionCallOutput {
                id: id.clone(),
                call_id: call_id.clone(),
                output: empty_output.clone(),
                internal_chat_message_metadata_passthrough:
                    internal_chat_message_metadata_passthrough.clone(),
            };
            let output_budget = embedded_payload_budget(&empty_item, &empty_output);
            ResponseItem::FunctionCallOutput {
                id,
                call_id,
                output: bound_function_output_payload(output, output_budget),
                internal_chat_message_metadata_passthrough,
            }
        }
        ResponseItem::CustomToolCallOutput {
            id,
            call_id,
            name,
            output,
            internal_chat_message_metadata_passthrough,
        } => {
            let empty_output = FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(String::new()),
                success: output.success,
            };
            let empty_item = ResponseItem::CustomToolCallOutput {
                id: id.clone(),
                call_id: call_id.clone(),
                name: name.clone(),
                output: empty_output.clone(),
                internal_chat_message_metadata_passthrough:
                    internal_chat_message_metadata_passthrough.clone(),
            };
            let output_budget = embedded_payload_budget(&empty_item, &empty_output);
            ResponseItem::CustomToolCallOutput {
                id,
                call_id,
                name,
                output: bound_function_output_payload(output, output_budget),
                internal_chat_message_metadata_passthrough,
            }
        }
        ResponseItem::ToolSearchOutput {
            id,
            call_id,
            status,
            execution,
            tools,
            internal_chat_message_metadata_passthrough,
        } => {
            let empty_tools = Vec::new();
            let empty_item = ResponseItem::ToolSearchOutput {
                id: id.clone(),
                call_id: call_id.clone(),
                status: status.clone(),
                execution: execution.clone(),
                tools: empty_tools.clone(),
                internal_chat_message_metadata_passthrough:
                    internal_chat_message_metadata_passthrough.clone(),
            };
            let tools_budget = embedded_payload_budget(&empty_item, &empty_tools);
            ResponseItem::ToolSearchOutput {
                id,
                call_id,
                status,
                execution,
                tools: bound_tool_values(tools, tools_budget),
                internal_chat_message_metadata_passthrough,
            }
        }
        item => item,
    };
    if is_tool_output_item(&bounded)
        && serialized_len(&bounded)
            .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES)
    {
        omitted_tool_output(bounded)
    } else {
        bounded
    }
}

/// Prevents inherited user configuration from raising the per-item tool-output threshold.
pub(crate) fn clamp_workflow_child_tool_output_limit(config: &mut Config) {
    if let Some(limit) = config.tool_output_token_limit.as_mut() {
        *limit = (*limit).min(MAX_WORKFLOW_CHILD_TOOL_OUTPUT_TOKENS);
    }
}

fn tool_array_with_candidate_fits(
    bounded: &[ToolSpec],
    candidate: &ToolSpec,
    aggregate_bytes: usize,
) -> bool {
    if serialized_len(candidate)
        .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)
    {
        return false;
    }
    let mut tools = Vec::with_capacity(bounded.len() + 1);
    tools.extend_from_slice(bounded);
    tools.push(candidate.clone());
    serialized_len(&tools).is_some_and(|bytes| bytes <= aggregate_bytes)
}

fn responses_lite_additional_tools_fits(tools: &[ToolSpec]) -> bool {
    let Ok(tools) = create_tools_json_for_responses_api(tools) else {
        return false;
    };
    let item = ResponseItem::AdditionalTools {
        id: None,
        role: "developer".to_string(),
        tools,
    };
    serialized_len(&item)
        .is_some_and(|bytes| bytes <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES)
}

fn embedded_payload_budget<T: Serialize>(empty_item: &ResponseItem, empty_payload: &T) -> usize {
    let Some(item_bytes) = serialized_len(empty_item) else {
        return 0;
    };
    let Some(payload_bytes) = serialized_len(empty_payload) else {
        return 0;
    };
    MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES
        .saturating_sub(item_bytes.saturating_sub(payload_bytes))
}

fn bound_function_output_payload(
    output: FunctionCallOutputPayload,
    max_serialized_bytes: usize,
) -> FunctionCallOutputPayload {
    let FunctionCallOutputPayload { body, success } = output;
    let body = match body {
        FunctionCallOutputBody::Text(text) => {
            let text = bound_text_for_serialization(
                text,
                max_serialized_bytes,
                |candidate| {
                    serialized_len(&FunctionCallOutputPayload {
                        body: FunctionCallOutputBody::Text(candidate.to_string()),
                        success,
                    })
                },
            );
            FunctionCallOutputBody::Text(text)
        }
        FunctionCallOutputBody::ContentItems(items) => {
            let had_items = !items.is_empty();
            let mut bounded_items = Vec::new();
            for item in items {
                if bounded_items.len() >= MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS {
                    break;
                }
                let Some(item) = bound_output_content_item(item) else {
                    continue;
                };
                let mut candidate_items = bounded_items.clone();
                candidate_items.push(item.clone());
                let candidate = FunctionCallOutputPayload {
                    body: FunctionCallOutputBody::ContentItems(candidate_items),
                    success,
                };
                if serialized_len(&candidate)
                    .is_none_or(|bytes| bytes > max_serialized_bytes)
                {
                    continue;
                }
                bounded_items.push(item);
            }
            if had_items && bounded_items.is_empty() {
                FunctionCallOutputBody::Text(OUTPUT_TRUNCATION_MARKER.to_string())
            } else {
                FunctionCallOutputBody::ContentItems(bounded_items)
            }
        }
    };
    FunctionCallOutputPayload { body, success }
}

fn bound_output_content_item(
    item: FunctionCallOutputContentItem,
) -> Option<FunctionCallOutputContentItem> {
    match item {
        FunctionCallOutputContentItem::InputText { text } => {
            let text = bound_text_for_serialization(
                text,
                MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES,
                |candidate| {
                    serialized_len(&FunctionCallOutputContentItem::InputText {
                        text: candidate.to_string(),
                    })
                },
            );
            Some(FunctionCallOutputContentItem::InputText { text })
        }
        item @ (FunctionCallOutputContentItem::InputImage { .. }
        | FunctionCallOutputContentItem::EncryptedContent { .. })
            if serialized_len(&item)
                .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES) =>
        {
            None
        }
        item => Some(item),
    }
}

fn bound_tool_values(
    tools: Vec<serde_json::Value>,
    max_serialized_bytes: usize,
) -> Vec<serde_json::Value> {
    let mut bounded = Vec::new();
    for tool in tools {
        if bounded.len() >= MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS {
            break;
        }
        if serialized_len(&tool)
            .is_none_or(|bytes| bytes > MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)
        {
            continue;
        }
        let mut candidate = bounded.clone();
        candidate.push(tool.clone());
        if serialized_len(&candidate)
            .is_none_or(|bytes| bytes > max_serialized_bytes)
        {
            continue;
        }
        bounded.push(tool);
    }
    bounded
}

fn is_tool_output_item(item: &ResponseItem) -> bool {
    matches!(
        item,
        ResponseItem::AdditionalTools { .. }
            | ResponseItem::FunctionCallOutput { .. }
            | ResponseItem::CustomToolCallOutput { .. }
            | ResponseItem::ToolSearchOutput { .. }
    )
}

fn omitted_tool_output(item: ResponseItem) -> ResponseItem {
    match item {
        ResponseItem::AdditionalTools { id, role, .. } => ResponseItem::AdditionalTools {
            id,
            role,
            tools: Vec::new(),
        },
        ResponseItem::FunctionCallOutput {
            id,
            call_id,
            output,
            internal_chat_message_metadata_passthrough,
        } => ResponseItem::FunctionCallOutput {
            id,
            call_id,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(OUTPUT_TRUNCATION_MARKER.to_string()),
                success: output.success,
            },
            internal_chat_message_metadata_passthrough,
        },
        ResponseItem::CustomToolCallOutput {
            id,
            call_id,
            name,
            output,
            internal_chat_message_metadata_passthrough,
        } => ResponseItem::CustomToolCallOutput {
            id,
            call_id,
            name,
            output: FunctionCallOutputPayload {
                body: FunctionCallOutputBody::Text(OUTPUT_TRUNCATION_MARKER.to_string()),
                success: output.success,
            },
            internal_chat_message_metadata_passthrough,
        },
        ResponseItem::ToolSearchOutput {
            id,
            call_id,
            status,
            execution,
            internal_chat_message_metadata_passthrough,
            ..
        } => ResponseItem::ToolSearchOutput {
            id,
            call_id,
            status,
            execution,
            tools: Vec::new(),
            internal_chat_message_metadata_passthrough,
        },
        item => item,
    }
}

fn is_omitted_tool_output(item: &ResponseItem) -> bool {
    match item {
        ResponseItem::AdditionalTools { tools, .. }
        | ResponseItem::ToolSearchOutput { tools, .. } => tools.is_empty(),
        ResponseItem::FunctionCallOutput { output, .. }
        | ResponseItem::CustomToolCallOutput { output, .. } => {
            output.text_content() == Some(OUTPUT_TRUNCATION_MARKER)
        }
        _ => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ToolCallKey {
    Function(String),
    Custom(String),
    Search(String),
}

fn tool_output_call_key(item: &ResponseItem) -> Option<ToolCallKey> {
    match item {
        ResponseItem::FunctionCallOutput { call_id, .. } => {
            Some(ToolCallKey::Function(call_id.clone()))
        }
        ResponseItem::CustomToolCallOutput { call_id, .. } => {
            Some(ToolCallKey::Custom(call_id.clone()))
        }
        ResponseItem::ToolSearchOutput {
            call_id: Some(call_id),
            ..
        } => Some(ToolCallKey::Search(call_id.clone())),
        ResponseItem::AdditionalTools { .. }
        | ResponseItem::ToolSearchOutput { call_id: None, .. }
        | ResponseItem::Message { .. }
        | ResponseItem::AgentMessage { .. }
        | ResponseItem::Reasoning { .. }
        | ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::CompactionTrigger { .. }
        | ResponseItem::ContextCompaction { .. }
        | ResponseItem::Other => None,
    }
}

fn remove_matching_call_from_batch(
    output: &ResponseItem,
    bounded: &mut Vec<ResponseItem>,
    output_positions: &mut [usize],
) {
    let Some(key) = tool_output_call_key(output) else {
        return;
    };
    let Some(position) = bounded.iter().position(|item| match (item, &key) {
        (ResponseItem::FunctionCall { call_id, .. }, ToolCallKey::Function(expected))
        | (ResponseItem::CustomToolCall { call_id, .. }, ToolCallKey::Custom(expected)) => {
            call_id == expected
        }
        (
            ResponseItem::ToolSearchCall {
                call_id: Some(call_id),
                ..
            },
            ToolCallKey::Search(expected),
        ) => call_id == expected,
        _ => false,
    }) else {
        return;
    };
    remove_bounded_item_at(bounded, output_positions, position);
}

fn remove_bounded_item_at(
    bounded: &mut Vec<ResponseItem>,
    output_positions: &mut [usize],
    position: usize,
) {
    bounded.remove(position);
    for output_position in output_positions {
        if *output_position > position {
            *output_position -= 1;
        }
    }
}

fn bound_text_for_serialization(
    text: String,
    max_serialized_bytes: usize,
    serialized_len_for: impl Fn(&str) -> Option<usize>,
) -> String {
    let max_bytes = if serialized_len_for(&text)
        .is_some_and(|bytes| bytes <= max_serialized_bytes)
    {
        return text;
    } else {
        text.len().saturating_sub(1)
    };
    let mut retained_bytes = max_bytes;
    loop {
        let candidate = truncate_text_with_marker(&text, retained_bytes);
        if serialized_len_for(&candidate)
            .is_some_and(|bytes| bytes <= max_serialized_bytes)
        {
            return candidate;
        }
        if retained_bytes == 0 {
            return String::new();
        }
        retained_bytes /= 2;
    }
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
    let mut bounded = String::with_capacity(retained_bytes + OUTPUT_TRUNCATION_MARKER.len());
    bounded.push_str(&text[..prefix_end]);
    bounded.push_str(OUTPUT_TRUNCATION_MARKER);
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

fn serialized_len(value: &impl Serialize) -> Option<usize> {
    serde_json::to_vec(value).ok().map(|bytes| bytes.len())
}

#[cfg(test)]
#[path = "workflow_child_tools_tests.rs"]
mod tests;

use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseItem;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiNamespace;
use codex_tools::ResponsesApiNamespaceTool;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolSpec;
use codex_tools::create_tools_json_for_responses_api;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

use super::MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS;
use super::MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES;
use super::MAX_WORKFLOW_CHILD_OUTPUT_BATCH_BYTES;
use super::MAX_WORKFLOW_CHILD_OUTPUT_ITEMS;
use super::MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES;
use super::MAX_WORKFLOW_CHILD_TOOL_SPECS;
use super::MAX_WORKFLOW_CHILD_TOOL_SPECS_BYTES;
use super::MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES;
use super::OUTPUT_TRUNCATION_MARKER;
use super::bound_workflow_child_output_item;
use super::bound_workflow_child_output_items;
use super::bound_workflow_child_responses_lite_tool_specs;
use super::bound_workflow_child_tool_specs;

fn function_spec(name: impl Into<String>, description: impl Into<String>) -> ToolSpec {
    ToolSpec::Function(ResponsesApiTool {
        name: name.into(),
        description: description.into(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::default(),
        output_schema: None,
    })
}

fn namespace_function(
    name: impl Into<String>,
    description: impl Into<String>,
) -> ResponsesApiNamespaceTool {
    ResponsesApiNamespaceTool::Function(ResponsesApiTool {
        name: name.into(),
        description: description.into(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::default(),
        output_schema: None,
    })
}

fn serialized_len(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value).expect("fixture should serialize").len()
}

#[test]
fn tool_specs_drop_oversized_entries_without_reordering_safe_entries() {
    let safe_before = function_spec("shell_command", "Run a shell command.");
    let oversized = function_spec(
        "hostile_dynamic_tool",
        "x".repeat(MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES),
    );
    let safe_after = function_spec("apply_patch", "Apply a patch.");

    let bounded = bound_workflow_child_tool_specs(vec![
        safe_before.clone(),
        oversized,
        safe_after.clone(),
    ]);

    assert_eq!(bounded, vec![safe_before, safe_after]);
    assert!(
        bounded
            .iter()
            .all(|tool| serialized_len(tool) <= MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)
    );
}

#[test]
fn tool_specs_enforce_logical_count_inside_namespaces() {
    let namespace = ToolSpec::Namespace(ResponsesApiNamespace {
        name: "codex_app".to_string(),
        description: "Application tools.".to_string(),
        tools: (0..MAX_WORKFLOW_CHILD_TOOL_SPECS + 8)
            .map(|index| namespace_function(format!("tool_{index:03}"), "small"))
            .collect(),
    });

    let bounded = bound_workflow_child_tool_specs(vec![namespace]);

    let [ToolSpec::Namespace(namespace)] = bounded.as_slice() else {
        panic!("expected one bounded namespace");
    };
    assert_eq!(namespace.tools.len(), MAX_WORKFLOW_CHILD_TOOL_SPECS);
    let names = namespace
        .tools
        .iter()
        .map(|tool| match tool {
            ResponsesApiNamespaceTool::Function(tool) => tool.name.as_str(),
        })
        .collect::<Vec<_>>();
    let expected = (0..MAX_WORKFLOW_CHILD_TOOL_SPECS)
        .map(|index| format!("tool_{index:03}"))
        .collect::<Vec<_>>();
    assert_eq!(names, expected.iter().map(String::as_str).collect::<Vec<_>>());
}

#[test]
fn tool_specs_enforce_exact_aggregate_serialized_cap() {
    let tools = (0..MAX_WORKFLOW_CHILD_TOOL_SPECS)
        .map(|index| function_spec(format!("aggregate_{index:03}"), "z".repeat(3_000)))
        .collect::<Vec<_>>();

    let bounded = bound_workflow_child_tool_specs(tools);

    assert!(!bounded.is_empty());
    assert!(bounded.len() < MAX_WORKFLOW_CHILD_TOOL_SPECS);
    assert!(serialized_len(&bounded) <= MAX_WORKFLOW_CHILD_TOOL_SPECS_BYTES);
    assert_eq!(bounded[0].name(), "aggregate_000");
    for (index, tool) in bounded.iter().enumerate() {
        assert_eq!(tool.name(), format!("aggregate_{index:03}"));
    }
}

#[test]
fn responses_lite_tool_array_fits_one_bounded_additional_tools_payload() {
    let tools = (0..MAX_WORKFLOW_CHILD_TOOL_SPECS)
        .map(|index| function_spec(format!("lite_{index:03}"), "z".repeat(1_000)))
        .collect::<Vec<_>>();

    let bounded = bound_workflow_child_responses_lite_tool_specs(tools);

    assert!(!bounded.is_empty());
    assert!(bounded.len() < MAX_WORKFLOW_CHILD_TOOL_SPECS);
    let item = ResponseItem::AdditionalTools {
        id: None,
        role: "developer".to_string(),
        tools: create_tools_json_for_responses_api(&bounded).expect("serialize bounded tools"),
    };
    assert!(serialized_len(&item) <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    assert_eq!(bounded[0].name(), "lite_000");
}

#[test]
fn namespace_drops_only_unsafe_nested_specs_and_preserves_safe_order() {
    let namespace = ToolSpec::Namespace(ResponsesApiNamespace {
        name: "mcp".to_string(),
        description: "MCP tools.".to_string(),
        tools: vec![
            namespace_function("safe_before", "small"),
            namespace_function(
                "unsafe",
                "x".repeat(MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES),
            ),
            namespace_function("safe_after", "small"),
        ],
    });

    let bounded = bound_workflow_child_tool_specs(vec![namespace]);

    let [ToolSpec::Namespace(namespace)] = bounded.as_slice() else {
        panic!("expected one bounded namespace");
    };
    let names = namespace
        .tools
        .iter()
        .map(|tool| match tool {
            ResponsesApiNamespaceTool::Function(tool) => tool.name.as_str(),
        })
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["safe_before", "safe_after"]);
}

#[test]
fn namespace_wrapper_respects_the_per_spec_serialized_cap() {
    let namespace = ToolSpec::Namespace(ResponsesApiNamespace {
        name: "mcp".to_string(),
        description: "MCP tools.".to_string(),
        tools: (0..MAX_WORKFLOW_CHILD_TOOL_SPECS)
            .map(|index| namespace_function(format!("tool_{index:03}"), "x".repeat(1_000)))
            .collect(),
    });

    let bounded = bound_workflow_child_tool_specs(vec![namespace]);

    let [bounded_namespace @ ToolSpec::Namespace(namespace)] = bounded.as_slice() else {
        panic!("expected one bounded namespace");
    };
    assert!(namespace.tools.len() < MAX_WORKFLOW_CHILD_TOOL_SPECS);
    assert!(serialized_len(bounded_namespace) <= MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES);
}

#[test]
fn function_output_text_has_a_hard_serialized_byte_cap() {
    let item = ResponseItem::FunctionCallOutput {
        id: None,
        call_id: "call-1".to_string(),
        output: FunctionCallOutputPayload::from_text("\0🦀".repeat(20_000)),
        internal_chat_message_metadata_passthrough: None,
    };

    let bounded = bound_workflow_child_output_item(item);

    assert!(serialized_len(&bounded) <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    let ResponseItem::FunctionCallOutput { output, .. } = bounded else {
        panic!("expected function output");
    };
    assert!(serialized_len(&output) <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    let FunctionCallOutputBody::Text(text) = output.body else {
        panic!("expected text output");
    };
    assert!(text.contains(OUTPUT_TRUNCATION_MARKER));
}

#[test]
fn structured_output_bounds_count_bytes_images_and_encrypted_content() {
    let item = ResponseItem::CustomToolCallOutput {
        id: None,
        call_id: "call-2".to_string(),
        name: Some("custom".to_string()),
        output: FunctionCallOutputPayload {
            body: FunctionCallOutputBody::ContentItems(vec![
                FunctionCallOutputContentItem::InputText {
                    text: "safe-before".to_string(),
                },
                FunctionCallOutputContentItem::InputImage {
                    image_url: format!(
                        "data:image/png;base64,{}",
                        "A".repeat(MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES)
                    ),
                    detail: None,
                },
                FunctionCallOutputContentItem::EncryptedContent {
                    encrypted_content: "E"
                        .repeat(MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES),
                },
                FunctionCallOutputContentItem::InputText {
                    text: "\0".repeat(MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES),
                },
            ]
            .into_iter()
            .chain((0..MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS + 8).map(|index| {
                FunctionCallOutputContentItem::InputText {
                    text: format!("tail-{index:03}"),
                }
            }))
            .collect()),
            success: Some(true),
        },
        internal_chat_message_metadata_passthrough: None,
    };

    let bounded = bound_workflow_child_output_item(item);

    assert!(serialized_len(&bounded) <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    let ResponseItem::CustomToolCallOutput { output, .. } = bounded else {
        panic!("expected custom output");
    };
    assert!(serialized_len(&output) <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    let FunctionCallOutputBody::ContentItems(items) = output.body else {
        panic!("expected structured output");
    };
    assert!(items.len() <= MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS);
    assert!(
        items
            .iter()
            .all(|item| serialized_len(item) <= MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEM_BYTES)
    );
    assert!(items.iter().all(|item| !matches!(
        item,
        FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. }
    )));
    let texts = items
        .iter()
        .map(|item| match item {
            FunctionCallOutputContentItem::InputText { text } => text.as_str(),
            FunctionCallOutputContentItem::InputImage { .. }
            | FunctionCallOutputContentItem::EncryptedContent { .. } => {
                unreachable!("unsafe non-text items should have been dropped")
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(texts[0], "safe-before");
    assert!(texts[1].contains(OUTPUT_TRUNCATION_MARKER));
    assert_eq!(texts[2], "tail-000");
}

#[test]
fn tool_search_output_bounds_each_tool_count_and_aggregate() {
    let oversized = json!({"name": "oversized", "description": "x".repeat(MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)});
    let tools = std::iter::once(json!({"name": "safe-before"}))
        .chain(std::iter::once(oversized))
        .chain((0..MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS + 8).map(|index| {
            json!({"name": format!("tool-{index:03}"), "description": "z".repeat(700)})
        }))
        .collect();
    let item = ResponseItem::ToolSearchOutput {
        id: None,
        call_id: Some("search-1".to_string()),
        status: "completed".to_string(),
        execution: "client".to_string(),
        tools,
        internal_chat_message_metadata_passthrough: None,
    };

    let bounded = bound_workflow_child_output_item(item);

    assert!(serialized_len(&bounded) <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    let ResponseItem::ToolSearchOutput { tools, .. } = bounded else {
        panic!("expected tool-search output");
    };
    assert!(tools.len() <= MAX_WORKFLOW_CHILD_OUTPUT_CONTENT_ITEMS);
    assert!(
        tools
            .iter()
            .all(|tool| serialized_len(tool) <= MAX_WORKFLOW_CHILD_TOOL_SPEC_BYTES)
    );
    assert!(serialized_len(&tools) <= MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    let names = tools
        .iter()
        .filter_map(|tool: &Value| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    assert_eq!(names.first(), Some(&"safe-before"));
    assert!(!names.contains(&"oversized"));
}

#[test]
fn output_boundary_preserves_model_generated_call_items() {
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "shell_command".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: "call-preserved".to_string(),
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(bound_workflow_child_output_item(call.clone()), call);
}

#[test]
fn output_batch_enforces_count_cap_and_preserves_source_order() {
    let mut inputs = Vec::new();
    for index in 0..MAX_WORKFLOW_CHILD_OUTPUT_ITEMS + 1 {
        let call_id = format!("call-{index:03}");
        inputs.push(ResponseItem::FunctionCall {
            id: None,
            name: "shell_command".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        });
        inputs.push(ResponseItem::FunctionCallOutput {
            id: None,
            call_id,
            output: FunctionCallOutputPayload::from_text("small".to_string()),
            internal_chat_message_metadata_passthrough: None,
        });
    }

    let bounded = bound_workflow_child_output_items(inputs);

    let call_ids = bounded
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let output_call_ids = bounded
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCallOutput { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let expected = (1..=MAX_WORKFLOW_CHILD_OUTPUT_ITEMS)
        .map(|index| format!("call-{index:03}"))
        .collect::<Vec<_>>();
    assert_eq!(
        call_ids,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(output_call_ids, call_ids);
    assert_eq!(output_call_ids.last(), Some(&"call-064"));
}

#[test]
fn output_batch_enforces_aggregate_cap_with_paired_omission_outputs() {
    let mut inputs = Vec::new();
    for index in 0..16 {
        let call_id = format!("paired-{index:03}");
        inputs.push(ResponseItem::FunctionCall {
            id: None,
            name: "shell_command".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: call_id.clone(),
            internal_chat_message_metadata_passthrough: None,
        });
        inputs.push(ResponseItem::FunctionCallOutput {
            id: None,
            call_id,
            output: FunctionCallOutputPayload::from_text("x".repeat(8 * 1024)),
            internal_chat_message_metadata_passthrough: None,
        });
    }

    let bounded = bound_workflow_child_output_items(inputs);

    let calls = bounded
        .iter()
        .filter_map(|item| match item {
            ResponseItem::FunctionCall { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let outputs = bounded
        .iter()
        .filter(|item| matches!(item, ResponseItem::FunctionCallOutput { .. }))
        .cloned()
        .collect::<Vec<_>>();
    let output_call_ids = outputs
        .iter()
        .map(|item| match item {
            ResponseItem::FunctionCallOutput { call_id, .. } => call_id.as_str(),
            _ => unreachable!("filtered to function outputs"),
        })
        .collect::<Vec<_>>();
    assert_eq!(output_call_ids, calls);
    assert!(serialized_len(&outputs) <= MAX_WORKFLOW_CHILD_OUTPUT_BATCH_BYTES);
    assert!(outputs.iter().any(|item| matches!(
        item,
        ResponseItem::FunctionCallOutput { output, .. }
            if output.text_content() == Some(OUTPUT_TRUNCATION_MARKER)
    )));
}

#[test]
fn output_batch_drops_a_pair_when_the_immutable_output_wrapper_is_oversized() {
    let call_id = "x".repeat(MAX_WORKFLOW_CHILD_OUTPUT_PAYLOAD_BYTES);
    let call = ResponseItem::FunctionCall {
        id: None,
        name: "shell_command".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: call_id.clone(),
        internal_chat_message_metadata_passthrough: None,
    };
    let output = ResponseItem::FunctionCallOutput {
        id: None,
        call_id,
        output: FunctionCallOutputPayload::from_text("small".to_string()),
        internal_chat_message_metadata_passthrough: None,
    };

    assert_eq!(
        bound_workflow_child_output_items([call, output]),
        Vec::<ResponseItem>::new()
    );
}

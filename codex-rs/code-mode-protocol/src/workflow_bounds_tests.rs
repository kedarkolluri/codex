use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

fn escaped_string_with_serialized_len(serialized_bytes: usize) -> String {
    let payload_bytes = serialized_bytes - 2;
    "\0".repeat(payload_bytes / 6) + &"x".repeat(payload_bytes % 6)
}

fn assert_bound(field: &str, max_bytes: usize, validate: impl Fn(&str) -> Result<(), String>) {
    let exact = "x".repeat(max_bytes);
    assert_eq!(validate(&exact), Ok(()));
    let over = format!("{exact}x");
    let actual = max_bytes + 1;
    let error = format!("{field} exceeds the {max_bytes}-byte limit (got {actual} bytes)");
    assert_eq!(validate(&over), Err(error));
}

#[test]
fn text_bounds_use_utf8_bytes_and_reject_empty_names() {
    let name = "é".repeat(WORKFLOW_NAME_MAX_BYTES / 2);
    assert_eq!(ensure_workflow_name(&name), Ok(()));
    assert!(ensure_workflow_name(&format!("{name}a")).is_err());
    assert!(ensure_workflow_name(" \n ").is_err());

    let prompt = "é".repeat(WORKFLOW_AGENT_PROMPT_MAX_BYTES / 2);
    assert_eq!(ensure_workflow_agent_prompt(&prompt), Ok(()));
    assert!(ensure_workflow_agent_prompt(&format!("{prompt}a")).is_err());
}

#[test]
fn workflow_args_bound_uses_serialized_json_bytes() {
    let serialized_string = escaped_string_with_serialized_len;
    let execution_exact = json!(serialized_string(WORKFLOW_ARGS_MAX_BYTES));
    let execution_over = json!(serialized_string(WORKFLOW_ARGS_MAX_BYTES + 1));
    assert_eq!(ensure_workflow_args(&execution_exact), Ok(()));
    assert!(ensure_workflow_args(&execution_over).is_err());

    let model_exact = json!(serialized_string(WORKFLOW_MODEL_ARGS_MAX_BYTES));
    let model_over = json!(serialized_string(WORKFLOW_MODEL_ARGS_MAX_BYTES + 1));
    assert_eq!(ensure_workflow_model_args(&model_exact), Ok(()));
    assert!(ensure_workflow_model_args(&model_over).is_err());

    let mut nested = json!(null);
    for _ in 1..WORKFLOW_ARGS_MAX_DEPTH {
        nested = json!([nested]);
    }
    assert_eq!(ensure_workflow_args(&nested), Ok(()));
    nested = json!([nested]);
    assert!(ensure_workflow_args(&nested).is_err());
}

#[test]
fn agent_schema_bound_covers_serialized_size_and_depth() {
    let exact_size = json!(escaped_string_with_serialized_len(
        WORKFLOW_AGENT_SCHEMA_MAX_BYTES
    ));
    let oversized = json!(escaped_string_with_serialized_len(
        WORKFLOW_AGENT_SCHEMA_MAX_BYTES + 1
    ));
    assert_eq!(ensure_workflow_agent_schema(&exact_size), Ok(()));
    assert!(ensure_workflow_agent_schema(&oversized).is_err());

    let mut nested = json!(null);
    for _ in 1..WORKFLOW_AGENT_SCHEMA_MAX_DEPTH {
        nested = json!([nested]);
    }
    assert_eq!(ensure_workflow_agent_schema(&nested), Ok(()));
    nested = json!([nested]);
    assert!(ensure_workflow_agent_schema(&nested).is_err());
}

#[test]
fn exported_text_validators_apply_their_exact_contracts() {
    let label = ensure_workflow_agent_label;
    let max = WORKFLOW_AGENT_LABEL_MAX_BYTES;
    assert_bound("workflow agent label", max, label);
    let option: fn(&str) -> Result<(), String> =
        |value| ensure_workflow_agent_option("opts.model", value);
    assert_bound("opts.model", WORKFLOW_AGENT_OPTION_MAX_BYTES, option);
    let phase = ensure_workflow_phase_title;
    let max = WORKFLOW_PHASE_TITLE_MAX_BYTES;
    assert_bound("workflow phase title", max, phase);
    let log = ensure_workflow_log_message;
    assert_bound("workflow log message", WORKFLOW_LOG_MESSAGE_MAX_BYTES, log);
}

fn text_item_with_serialized_len(serialized_bytes: usize) -> FunctionCallOutputContentItem {
    FunctionCallOutputContentItem::InputText {
        text: escaped_string_with_serialized_len(serialized_bytes),
    }
}

fn assert_authored_payload_boundary(make_item: impl Fn(String) -> FunctionCallOutputContentItem) {
    let exact_item = [make_item(escaped_string_with_serialized_len(
        WORKFLOW_OUTPUT_ITEM_MAX_BYTES,
    ))];
    let mut exact = WorkflowOutputBounds::default();
    assert_eq!(exact.admit(&exact_item), Ok(()));
    assert_eq!(
        exact,
        WorkflowOutputBounds {
            item_count: 1,
            serialized_bytes: WORKFLOW_OUTPUT_ITEM_MAX_BYTES,
        }
    );

    let oversized_item = [make_item(escaped_string_with_serialized_len(
        WORKFLOW_OUTPUT_ITEM_MAX_BYTES + 1,
    ))];
    let mut oversized = WorkflowOutputBounds::default();
    assert_eq!(
        oversized.admit(&oversized_item),
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert_eq!(oversized, WorkflowOutputBounds::default());
}

#[test]
fn workflow_output_per_item_bound_covers_text_image_and_audio_payloads() {
    assert_authored_payload_boundary(|text| FunctionCallOutputContentItem::InputText { text });
    assert_authored_payload_boundary(|image_url| FunctionCallOutputContentItem::InputImage {
        image_url,
        detail: Some(crate::ImageDetail::Original),
    });
    assert_authored_payload_boundary(|audio_url| FunctionCallOutputContentItem::InputAudio {
        audio_url,
    });
}

#[test]
fn workflow_output_per_item_bound_covers_terminal_errors() {
    let exact_error = escaped_string_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES);
    let mut exact = WorkflowOutputBounds::default();
    assert_eq!(exact.admit_response(&[], Some(&exact_error)), Ok(()));
    assert_eq!(
        exact,
        WorkflowOutputBounds {
            item_count: 1,
            serialized_bytes: WORKFLOW_OUTPUT_ITEM_MAX_BYTES,
        }
    );

    let oversized_error = escaped_string_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES + 1);
    let mut oversized = WorkflowOutputBounds::default();
    assert_eq!(
        oversized.admit_response(&[], Some(&oversized_error)),
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert_eq!(oversized, WorkflowOutputBounds::default());
}

#[test]
fn workflow_output_bounds_enforce_the_aggregate_lifetime_cap() {
    let exact_item_count = WORKFLOW_OUTPUT_MAX_BYTES / WORKFLOW_OUTPUT_ITEM_MAX_BYTES;
    let exact_items =
        vec![text_item_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES); exact_item_count];
    let mut exact = WorkflowOutputBounds::default();
    assert_eq!(exact.admit(&exact_items), Ok(()));
    assert_eq!(
        exact,
        WorkflowOutputBounds {
            item_count: exact_item_count,
            serialized_bytes: WORKFLOW_OUTPUT_MAX_BYTES,
        }
    );

    let mut exact_with_error = WorkflowOutputBounds::default();
    assert_eq!(
        exact_with_error.admit(&exact_items[..exact_item_count - 1]),
        Ok(())
    );
    let exact_error = escaped_string_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES);
    assert_eq!(
        exact_with_error.admit_response(&[], Some(&exact_error)),
        Ok(())
    );
    assert_eq!(
        exact_with_error,
        WorkflowOutputBounds {
            item_count: exact_item_count,
            serialized_bytes: WORKFLOW_OUTPUT_MAX_BYTES,
        }
    );

    let mut one_under_items =
        vec![text_item_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES); exact_item_count - 1];
    one_under_items.push(text_item_with_serialized_len(
        WORKFLOW_OUTPUT_ITEM_MAX_BYTES - 1,
    ));
    let mut one_under = WorkflowOutputBounds::default();
    assert_eq!(one_under.admit(&one_under_items), Ok(()));
    let admitted = WorkflowOutputBounds {
        item_count: exact_item_count,
        serialized_bytes: WORKFLOW_OUTPUT_MAX_BYTES - 1,
    };
    assert_eq!(one_under, admitted);

    let one_byte_over = [text_item_with_serialized_len(2)];
    assert_eq!(
        one_under.admit(&one_byte_over),
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert_eq!(one_under, admitted);
    assert_eq!(
        one_under.admit_response(&[], Some("")),
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert_eq!(one_under, admitted);
}

#[test]
fn workflow_output_item_cap_counts_terminal_errors() {
    let empty_items = vec![text_item_with_serialized_len(2); WORKFLOW_OUTPUT_MAX_ITEMS - 1];
    let mut bounds = WorkflowOutputBounds::default();
    assert_eq!(bounds.admit_response(&empty_items, Some("")), Ok(()));
    let full = WorkflowOutputBounds {
        item_count: WORKFLOW_OUTPUT_MAX_ITEMS,
        serialized_bytes: WORKFLOW_OUTPUT_MAX_ITEMS * 2,
    };
    assert_eq!(bounds, full);

    assert_eq!(
        bounds.admit_response(&[], Some("")),
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert_eq!(bounds, full);
}

#[test]
fn workflow_output_later_item_and_error_failures_are_transactional() {
    let first = [FunctionCallOutputContentItem::InputText {
        text: "first".to_string(),
    }];
    let mut item_bounds = WorkflowOutputBounds::default();
    assert_eq!(item_bounds.admit(&first), Ok(()));
    let admitted = WorkflowOutputBounds {
        item_count: 1,
        serialized_bytes: "first".len() + 2,
    };
    assert_eq!(item_bounds, admitted);

    let later_oversized_item = [
        text_item_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES),
        text_item_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES + 1),
    ];
    assert_eq!(
        item_bounds.admit(&later_oversized_item),
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert_eq!(item_bounds, admitted);

    let response_item = [text_item_with_serialized_len(
        WORKFLOW_OUTPUT_ITEM_MAX_BYTES,
    )];
    let oversized_error = escaped_string_with_serialized_len(WORKFLOW_OUTPUT_ITEM_MAX_BYTES + 1);
    let mut error_bounds = WorkflowOutputBounds::default();
    assert_eq!(error_bounds.admit(&first), Ok(()));
    assert_eq!(error_bounds, admitted);
    assert_eq!(
        error_bounds.admit_response(&response_item, Some(&oversized_error)),
        Err(SAVED_WORKFLOW_OUTPUT_REJECTED.to_string())
    );
    assert_eq!(error_bounds, admitted);
}

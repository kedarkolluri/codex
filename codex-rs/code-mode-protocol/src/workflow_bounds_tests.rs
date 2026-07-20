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

#[test]
fn workflow_output_bounds_admit_chunks_transactionally() {
    let mut bounds = WorkflowOutputBounds::default();
    let first = [FunctionCallOutputContentItem::InputText {
        text: "first".to_string(),
    }];
    assert_eq!(bounds.admit(&first), Ok(()));
    let first_bytes = "first".len() + 2;

    let image_url = "data:image/png;base64,YQ==";
    let audio_url = "data:audio/wav;base64,YQ==";
    let media = [
        FunctionCallOutputContentItem::InputImage {
            image_url: image_url.to_string(),
            detail: None,
        },
        FunctionCallOutputContentItem::InputAudio {
            audio_url: audio_url.to_string(),
        },
    ];
    assert_eq!(bounds.admit(&media), Ok(()));
    let admitted = WorkflowOutputBounds {
        item_count: 3,
        serialized_bytes: first_bytes + image_url.len() + audio_url.len() + 4,
    };

    let oversized_chunk = [
        FunctionCallOutputContentItem::InputText {
            text: "must not commit".to_string(),
        },
        FunctionCallOutputContentItem::InputAudio {
            audio_url: "x".repeat(WORKFLOW_OUTPUT_MAX_BYTES),
        },
    ];
    let error = bounds
        .admit(&oversized_chunk)
        .expect_err("a later oversized item must reject the entire chunk");
    assert_eq!(error, output_byte_limit_error());
    assert_eq!(
        bounds, admitted,
        "a rejected chunk must not partially mutate aggregate accounting"
    );
}

#[test]
fn workflow_output_bounds_enforce_exact_payload_and_item_caps() {
    let exact_payload = [FunctionCallOutputContentItem::InputText {
        text: escaped_string_with_serialized_len(WORKFLOW_OUTPUT_MAX_BYTES),
    }];
    let mut exact = WorkflowOutputBounds::default();
    assert_eq!(exact.admit(&exact_payload), Ok(()));
    let exact_full = WorkflowOutputBounds {
        item_count: 1,
        serialized_bytes: WORKFLOW_OUTPUT_MAX_BYTES,
    };
    assert_eq!(exact, exact_full);

    let overflow = [FunctionCallOutputContentItem::InputText {
        text: String::new(),
    }];
    assert_eq!(exact.admit(&overflow), Err(output_byte_limit_error()));
    assert_eq!(exact, exact_full);

    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: String::new(),
        };
        WORKFLOW_OUTPUT_MAX_ITEMS
    ];
    let mut count = WorkflowOutputBounds::default();
    assert_eq!(count.admit(&items), Ok(()));
    let count_full = WorkflowOutputBounds {
        item_count: WORKFLOW_OUTPUT_MAX_ITEMS,
        serialized_bytes: WORKFLOW_OUTPUT_MAX_ITEMS * 2,
    };
    assert_eq!(count, count_full);
    assert_eq!(count.admit(&overflow), Err(output_item_limit_error()));
    assert_eq!(count, count_full);
}

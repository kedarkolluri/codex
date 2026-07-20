use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn workflow_name_bound_is_exact_and_rejects_empty_input() {
    assert_eq!(
        ensure_workflow_name(&"a".repeat(WORKFLOW_NAME_MAX_BYTES)),
        Ok(())
    );
    assert!(ensure_workflow_name(&"a".repeat(WORKFLOW_NAME_MAX_BYTES + 1)).is_err());
    assert!(ensure_workflow_name(" \n ").is_err());
}

#[test]
fn metadata_bounds_cover_description_phase_count_and_titles() {
    let valid = ParsedWorkflowMeta {
        name: "bounded".to_string(),
        description: "d".repeat(WORKFLOW_DESCRIPTION_MAX_BYTES),
        phases: vec!["p".repeat(WORKFLOW_PHASE_TITLE_MAX_BYTES)],
    };
    assert_eq!(ensure_parsed_workflow_meta(&valid), Ok(()));

    let mut too_many = valid.clone();
    too_many.phases = vec!["phase".to_string(); WORKFLOW_PHASES_MAX_ITEMS + 1];
    assert!(ensure_parsed_workflow_meta(&too_many).is_err());

    let mut long_title = valid;
    long_title.phases = vec!["p".repeat(WORKFLOW_PHASE_TITLE_MAX_BYTES + 1)];
    assert!(ensure_parsed_workflow_meta(&long_title).is_err());
}

#[test]
fn static_parser_applies_metadata_bounds() {
    let source = format!(
        "export const meta = {{ name: '{}', description: 'bounded' }};",
        "n".repeat(WORKFLOW_NAME_MAX_BYTES + 1)
    );
    assert!(crate::parse_workflow_meta(&source).is_err());

    let phases = std::iter::repeat_n("'phase'", WORKFLOW_PHASES_MAX_ITEMS + 1)
        .collect::<Vec<_>>()
        .join(",");
    let source = format!(
        "export const meta = {{ name: 'bounded', description: 'bounded', phases: [{phases}] }};"
    );
    assert!(crate::parse_workflow_meta(&source).is_err());
}

#[test]
fn workflow_args_bound_uses_serialized_json_bytes() {
    assert_eq!(ensure_workflow_args(&json!({"ok": true})), Ok(()));
    assert!(ensure_workflow_args(&json!("x".repeat(WORKFLOW_ARGS_MAX_BYTES))).is_err());
    assert_eq!(ensure_workflow_model_args(&json!({"ok": true})), Ok(()));
    assert!(ensure_workflow_model_args(&json!("x".repeat(WORKFLOW_MODEL_ARGS_MAX_BYTES))).is_err());
}

#[test]
fn agent_prompt_transport_bound_is_exact() {
    assert_eq!(
        ensure_workflow_agent_prompt(&"a".repeat(WORKFLOW_AGENT_PROMPT_MAX_BYTES)),
        Ok(())
    );
    assert!(
        ensure_workflow_agent_prompt(&"a".repeat(WORKFLOW_AGENT_PROMPT_MAX_BYTES + 1)).is_err()
    );
}

#[test]
fn agent_schema_bound_covers_serialized_size_and_depth() {
    assert_eq!(
        ensure_workflow_agent_schema(&json!({"type": "object"})),
        Ok(())
    );
    assert!(
        ensure_workflow_agent_schema(&json!({
            "description": "x".repeat(WORKFLOW_AGENT_SCHEMA_MAX_BYTES)
        }))
        .is_err()
    );

    let mut nested = json!(null);
    for _ in 0..=WORKFLOW_AGENT_SCHEMA_MAX_DEPTH {
        nested = json!([nested]);
    }
    assert!(ensure_workflow_agent_schema(&nested).is_err());
}

#[test]
fn live_narration_bounds_are_exact() {
    assert_eq!(
        ensure_workflow_phase_title(&"p".repeat(WORKFLOW_PHASE_TITLE_MAX_BYTES)),
        Ok(())
    );
    assert!(ensure_workflow_phase_title(&"p".repeat(WORKFLOW_PHASE_TITLE_MAX_BYTES + 1)).is_err());
    assert_eq!(
        ensure_workflow_log_message(&"l".repeat(WORKFLOW_LOG_MESSAGE_MAX_BYTES)),
        Ok(())
    );
    assert!(ensure_workflow_log_message(&"l".repeat(WORKFLOW_LOG_MESSAGE_MAX_BYTES + 1)).is_err());
}

#[test]
fn workflow_output_bounds_admit_chunks_transactionally() {
    let mut bounds = WorkflowOutputBounds::default();
    let first = [FunctionCallOutputContentItem::InputText {
        text: "first".to_string(),
    }];
    assert_eq!(bounds.admit(&first), Ok(()));
    let admitted = bounds;

    let oversized_image = [FunctionCallOutputContentItem::InputImage {
        image_url: format!(
            "data:image/png;base64,{}",
            "x".repeat(WORKFLOW_OUTPUT_MAX_BYTES)
        ),
        detail: None,
    }];
    let error = bounds
        .admit(&oversized_image)
        .expect_err("image data payload must count toward the aggregate cap");
    assert!(error.contains("byte cap exceeded"));
    assert_eq!(
        bounds, admitted,
        "a rejected chunk must not partially mutate aggregate accounting"
    );

    let second = [FunctionCallOutputContentItem::InputText {
        text: "second".to_string(),
    }];
    assert_eq!(bounds.admit(&second), Ok(()));
    assert_eq!(bounds.item_count(), 2);
    assert!(bounds.serialized_bytes() > 0);
}

#[test]
fn workflow_output_bounds_enforce_exact_payload_and_item_caps() {
    let exact_payload = [FunctionCallOutputContentItem::InputText {
        // JSON string serialization contributes the surrounding two quotes.
        text: "x".repeat(WORKFLOW_OUTPUT_MAX_BYTES - 2),
    }];
    let mut exact = WorkflowOutputBounds::default();
    assert_eq!(exact.admit(&exact_payload), Ok(()));
    assert_eq!(exact.serialized_bytes(), WORKFLOW_OUTPUT_MAX_BYTES);

    let overflow = [FunctionCallOutputContentItem::InputText {
        text: String::new(),
    }];
    assert!(exact.admit(&overflow).is_err());

    let items = vec![
        FunctionCallOutputContentItem::InputText {
            text: String::new(),
        };
        WORKFLOW_OUTPUT_MAX_ITEMS
    ];
    let mut count = WorkflowOutputBounds::default();
    assert_eq!(count.admit(&items), Ok(()));
    assert_eq!(count.item_count(), WORKFLOW_OUTPUT_MAX_ITEMS);
    assert!(count.admit(&overflow).is_err());
}

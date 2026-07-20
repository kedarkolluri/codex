use super::*;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn prompt_within_token_and_transport_caps_is_accepted() {
    let empty_envelope_bytes = serde_json::to_vec(&workflow_agent_prompt_message(""))
        .expect("serialize workflow prompt envelope")
        .len();
    let prompt =
        "a".repeat(crate::context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES - empty_envelope_bytes);

    assert_eq!(ensure_prompt_within_bounds(&prompt), Ok(()));
    assert_eq!(
        ensure_prompt_within_bounds(&format!("{prompt}a")),
        Err(format!(
            "agent() prompt serialized envelope exceeds the {}-byte limit",
            crate::context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
        ))
    );
}

#[test]
fn oversized_prompt_is_rejected_with_bounded_diagnostic() {
    let prompt = "a".repeat(WORKFLOW_AGENT_PROMPT_MAX_TOKENS * 4 + 1);

    assert_eq!(
        ensure_prompt_within_bounds(&prompt),
        Err(format!(
            "agent() prompt is too large ({} estimated tokens > {WORKFLOW_AGENT_PROMPT_MAX_TOKENS} token cap)",
            WORKFLOW_AGENT_PROMPT_MAX_TOKENS + 1
        ))
    );
}

#[test]
fn multibyte_prompt_respects_the_utf8_transport_bound() {
    let empty_envelope_bytes = serde_json::to_vec(&workflow_agent_prompt_message(""))
        .expect("serialize workflow prompt envelope")
        .len();
    let prompt = "é"
        .repeat((crate::context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES - empty_envelope_bytes) / 2);

    assert_eq!(ensure_prompt_within_bounds(&prompt), Ok(()));
    assert!(ensure_prompt_within_bounds(&format!("{prompt}é")).is_err());
}

#[test]
fn prompt_admission_charges_json_escape_expansion() {
    let prompt = "\0".repeat(2 * 1024);

    assert!(prompt.len() < codex_code_mode::WORKFLOW_AGENT_PROMPT_MAX_BYTES);
    assert_eq!(
        ensure_prompt_within_bounds(&prompt),
        Err(format!(
            "agent() prompt serialized envelope exceeds the {}-byte limit",
            crate::context::MAX_WORKFLOW_CHILD_CONTEXT_ITEM_BYTES
        ))
    );
}

#[test]
fn agent_string_options_are_bounded() {
    assert_eq!(
        ensure_agent_options_within_bounds(
            Some("reviewer"),
            Some("review"),
            Some("model"),
            Some("high"),
            Some("reviewer"),
            Some("worktree"),
        ),
        Ok(())
    );
    assert!(
        ensure_agent_options_within_bounds(
            Some(&"x".repeat(codex_code_mode::WORKFLOW_AGENT_LABEL_MAX_BYTES + 1)),
            None,
            None,
            None,
            None,
            None,
        )
        .is_err()
    );
}

#[test]
fn small_schema_is_within_bounds() {
    let schema = json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
    });

    assert_eq!(ensure_schema_within_bounds(&schema), Ok(()));
}

#[test]
fn oversized_schema_is_rejected() {
    let mut properties = serde_json::Map::new();
    for index in 0..4_000 {
        properties.insert(format!("field_{index}"), json!({ "type": "string" }));
    }
    let schema = json!({ "type": "object", "properties": properties });

    let error = ensure_schema_within_bounds(&schema).expect_err("oversized schema must reject");

    assert!(error.contains("too large"), "unexpected error: {error}");
}

#[test]
fn overdeep_schema_is_rejected() {
    let mut node = json!({ "type": "string" });
    for _ in 0..(codex_code_mode::WORKFLOW_AGENT_SCHEMA_MAX_DEPTH + 5) {
        node = json!({ "type": "object", "properties": { "a": node } });
    }

    let error = ensure_schema_within_bounds(&node).expect_err("overdeep schema must reject");

    assert!(error.contains("too deep"), "unexpected error: {error}");
}

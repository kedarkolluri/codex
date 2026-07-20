use super::*;
use pretty_assertions::assert_eq;

#[test]
fn prompt_payload_is_byte_bounded() {
    let exactly_one_field_limit = MultiAgentV2Config {
        usage_hint_text: Some("a".repeat(PROMPT_FIELD_MAX_BYTES)),
        root_agent_usage_hint_text: None,
        subagent_usage_hint_text: None,
        multi_agent_mode_hint_text: None,
        ..Default::default()
    };
    validate(&exactly_one_field_limit).expect("the exact per-field byte limit should be accepted");

    let over_one_field_limit = MultiAgentV2Config {
        usage_hint_text: Some("a".repeat(PROMPT_FIELD_MAX_BYTES + 1)),
        root_agent_usage_hint_text: None,
        subagent_usage_hint_text: None,
        multi_agent_mode_hint_text: None,
        ..Default::default()
    };
    assert_eq!(
        validate(&over_one_field_limit)
            .expect_err("an oversized prompt field should be rejected")
            .to_string(),
        "features.multi_agent_v2.usage_hint_text exceeds the 4000-byte limit (got 4001 bytes)"
    );

    let over_combined_limit = MultiAgentV2Config {
        usage_hint_text: Some("a".repeat(PROMPT_FIELD_MAX_BYTES)),
        root_agent_usage_hint_text: Some("b".repeat(PROMPT_FIELD_MAX_BYTES)),
        subagent_usage_hint_text: Some("c".to_string()),
        multi_agent_mode_hint_text: None,
        ..Default::default()
    };
    assert_eq!(
        validate(&over_combined_limit)
            .expect_err("an oversized aggregate prompt payload should be rejected")
            .to_string(),
        "features.multi_agent_v2 prompt payload exceeds the 8000-byte combined limit (got 8001 bytes)"
    );
}

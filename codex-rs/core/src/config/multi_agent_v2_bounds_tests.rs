use super::*;
use crate::config::CONFIG_TOML_FILE;
use crate::config::ConfigBuilder;
use crate::config::ConfigToml;
use crate::config::validate_feature_dependencies_for_config_toml;
use codex_features::Feature;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

fn config_with_prompt_field(field: &str, value: String) -> MultiAgentV2Config {
    let mut config = MultiAgentV2Config {
        usage_hint_text: None,
        root_agent_usage_hint_text: None,
        subagent_usage_hint_text: None,
        multi_agent_mode_hint_text: None,
        ..Default::default()
    };
    match field {
        "usage_hint_text" => config.usage_hint_text = Some(value),
        "root_agent_usage_hint_text" => config.root_agent_usage_hint_text = Some(value),
        "subagent_usage_hint_text" => config.subagent_usage_hint_text = Some(value),
        "multi_agent_mode_hint_text" => config.multi_agent_mode_hint_text = Some(value),
        _ => panic!("unknown prompt field `{field}`"),
    }
    config
}

#[test]
fn prompt_fields_are_individually_byte_bounded() {
    for (field, max_bytes) in [
        ("usage_hint_text", USAGE_HINT_TEXT_MAX_BYTES),
        ("root_agent_usage_hint_text", PROMPT_FIELD_MAX_BYTES),
        ("subagent_usage_hint_text", PROMPT_FIELD_MAX_BYTES),
        ("multi_agent_mode_hint_text", PROMPT_FIELD_MAX_BYTES),
    ] {
        validate(&config_with_prompt_field(field, "a".repeat(max_bytes)))
            .expect("the exact per-field byte limit should be accepted");

        let over_limit = "é".repeat(max_bytes / "é".len() + 1);
        let over_limit_bytes = over_limit.len();
        assert_eq!(
            validate(&config_with_prompt_field(field, over_limit))
                .expect_err("an oversized prompt field should be rejected")
                .to_string(),
            format!(
                "features.multi_agent_v2.{field} exceeds the {max_bytes}-byte limit (got {over_limit_bytes} bytes)"
            )
        );
    }
}

#[test]
fn aggregate_prompt_payload_is_byte_bounded() {
    let exactly_at_limit = MultiAgentV2Config {
        usage_hint_text: Some("u".repeat(USAGE_HINT_TEXT_MAX_BYTES)),
        root_agent_usage_hint_text: Some("r".repeat(2_333)),
        subagent_usage_hint_text: Some("s".repeat(2_333)),
        multi_agent_mode_hint_text: Some("m".repeat(2_334)),
        ..Default::default()
    };
    validate(&exactly_at_limit).expect("the exact aggregate byte limit should be accepted");

    let over_limit = MultiAgentV2Config {
        multi_agent_mode_hint_text: Some("m".repeat(2_335)),
        ..exactly_at_limit
    };
    assert_eq!(
        validate(&over_limit)
            .expect_err("an oversized aggregate prompt payload should be rejected")
            .to_string(),
        "features.multi_agent_v2 prompt payload exceeds the 8000-byte combined limit (got 8001 bytes)"
    );
}

#[test]
fn effective_feature_validation_rejects_oversized_workflow_prompts() {
    let usage_hint_text = "u".repeat(USAGE_HINT_TEXT_MAX_BYTES + 1);
    let config: ConfigToml = toml::from_str(&format!(
        r#"[features]
workflow = true

[features.multi_agent_v2]
usage_hint_text = "{usage_hint_text}"
"#,
    ))
    .expect("valid config TOML");

    let error =
        validate_feature_dependencies_for_config_toml(&config, /*feature_requirements*/ None)
            .expect_err("effective config writes should reject oversized workflow prompts");
    assert_eq!(
        (error.kind(), error.to_string()),
        (
            std::io::ErrorKind::InvalidInput,
            "features.multi_agent_v2.usage_hint_text exceeds the 1000-byte limit (got 1001 bytes)"
                .to_string(),
        )
    );
}

#[tokio::test]
async fn multi_agent_v2_prompt_bounds_are_inactive_without_workflow() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let usage_hint_text = "é".repeat(USAGE_HINT_TEXT_MAX_BYTES / 2 + 1);
    let root_agent_usage_hint_text = "r".repeat(PROMPT_FIELD_MAX_BYTES);
    let subagent_usage_hint_text = "s".repeat(PROMPT_FIELD_MAX_BYTES);
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[features]
workflow = false

[features.multi_agent_v2]
enabled = true
usage_hint_text = "{usage_hint_text}"
root_agent_usage_hint_text = "{root_agent_usage_hint_text}"
subagent_usage_hint_text = "{subagent_usage_hint_text}"
"#,
        ),
    )?;

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await?;

    assert!(config.features.enabled(Feature::MultiAgentV2));
    assert!(!config.features.enabled(Feature::Workflow));
    assert_eq!(
        (
            config.multi_agent_v2.usage_hint_text.as_deref(),
            config.multi_agent_v2.root_agent_usage_hint_text.as_deref(),
            config.multi_agent_v2.subagent_usage_hint_text.as_deref(),
        ),
        (
            Some(usage_hint_text.as_str()),
            Some(root_agent_usage_hint_text.as_str()),
            Some(subagent_usage_hint_text.as_str()),
        )
    );

    Ok(())
}

#[tokio::test]
async fn workflow_rejects_multi_agent_v2_prompt_field_over_utf8_byte_limit() -> std::io::Result<()>
{
    let codex_home = TempDir::new()?;
    let usage_hint_text = "é".repeat(USAGE_HINT_TEXT_MAX_BYTES / 2 + 1);
    let usage_hint_bytes = usage_hint_text.len();
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[features]
workflow = true

[features.multi_agent_v2]
usage_hint_text = "{usage_hint_text}"
"#,
        ),
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("workflow should reject a prompt field over its UTF-8 byte limit");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        format!(
            "features.multi_agent_v2.usage_hint_text exceeds the 1000-byte limit (got {usage_hint_bytes} bytes)"
        )
    );

    Ok(())
}

#[tokio::test]
async fn workflow_rejects_combined_multi_agent_v2_prompt_payload() -> std::io::Result<()> {
    let codex_home = TempDir::new()?;
    let usage_hint_text = "u".repeat(USAGE_HINT_TEXT_MAX_BYTES);
    let root_agent_usage_hint_text = "r".repeat(2_500);
    let subagent_usage_hint_text = "s".repeat(2_500);
    let multi_agent_mode_hint_text = "m".repeat(2_001);
    let total_bytes = usage_hint_text.len()
        + root_agent_usage_hint_text.len()
        + subagent_usage_hint_text.len()
        + multi_agent_mode_hint_text.len();
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        format!(
            r#"[features]
workflow = true

[features.multi_agent_v2]
usage_hint_text = "{usage_hint_text}"
root_agent_usage_hint_text = "{root_agent_usage_hint_text}"
subagent_usage_hint_text = "{subagent_usage_hint_text}"
multi_agent_mode_hint_text = "{multi_agent_mode_hint_text}"
"#,
        ),
    )?;

    let err = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .build()
        .await
        .expect_err("workflow should reject an oversized combined prompt payload");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert_eq!(
        err.to_string(),
        format!(
            "features.multi_agent_v2 prompt payload exceeds the 8000-byte combined limit (got {total_bytes} bytes)"
        )
    );

    Ok(())
}

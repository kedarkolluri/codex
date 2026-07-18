use super::canonicalize;
use super::config_schema_json;
use super::write_config_schema;

use pretty_assertions::assert_eq;
use similar::TextDiff;
use tempfile::TempDir;

fn trim_single_trailing_newline(contents: &str) -> &str {
    contents.strip_suffix('\n').unwrap_or(contents)
}

#[test]
fn config_schema_matches_fixture() {
    let fixture_path = codex_utils_cargo_bin::find_resource!("config.schema.json")
        .expect("resolve config schema fixture path");
    let fixture = std::fs::read_to_string(fixture_path).expect("read config schema fixture");
    let fixture_value: serde_json::Value =
        serde_json::from_str(&fixture).expect("parse config schema fixture");
    let schema_json = config_schema_json().expect("serialize config schema");
    let schema_value: serde_json::Value =
        serde_json::from_slice(&schema_json).expect("decode schema json");
    let fixture_value = canonicalize(&fixture_value);
    let schema_value = canonicalize(&schema_value);
    if fixture_value != schema_value {
        let expected =
            serde_json::to_string_pretty(&fixture_value).expect("serialize fixture json");
        let actual = serde_json::to_string_pretty(&schema_value).expect("serialize schema json");
        let diff = TextDiff::from_lines(&expected, &actual)
            .unified_diff()
            .header("fixture", "generated")
            .to_string();
        panic!(
            "Current schema for `config.toml` doesn't match the fixture. \
Run `just write-config-schema` to overwrite with your changes.\n\n{diff}"
        );
    }

    // Make sure the version in the repo matches exactly: https://github.com/openai/codex/pull/10977.
    let tmp = TempDir::new().expect("create temp dir");
    let tmp_path = tmp.path().join("config.schema.json");
    write_config_schema(&tmp_path).expect("write config schema to temp path");
    let tmp_contents =
        std::fs::read_to_string(&tmp_path).expect("read back config schema from temp path");
    #[cfg(windows)]
    let fixture = fixture.replace("\r\n", "\n");
    #[cfg(windows)]
    let tmp_contents = tmp_contents.replace("\r\n", "\n");

    assert_eq!(
        trim_single_trailing_newline(&fixture),
        trim_single_trailing_newline(&tmp_contents),
        "fixture should match exactly with generated schema"
    );
}

#[test]
fn config_schema_accepts_workflow_feature_config() {
    let schema_json = config_schema_json().expect("serialize config schema");
    let schema_value: serde_json::Value =
        serde_json::from_slice(&schema_json).expect("decode schema json");

    // `[features.workflow]` must be wired as a typed property (not rejected by
    // the `additionalProperties: false` guard on the `[features]` table).
    let workflow = schema_value
        .pointer("/properties/features/properties/workflow")
        .expect("features.workflow property should exist in the schema");
    assert_eq!(
        workflow.get("$ref").and_then(serde_json::Value::as_str),
        Some("#/definitions/FeatureToml_for_WorkflowConfigToml"),
        "features.workflow should reference the typed FeatureToml wrapper"
    );

    // The wrapper accepts either a bare boolean or the config table form.
    let wrapper = schema_value
        .pointer("/definitions/FeatureToml_for_WorkflowConfigToml/anyOf")
        .and_then(serde_json::Value::as_array)
        .expect("FeatureToml_for_WorkflowConfigToml should be an anyOf");
    let accepts_bool = wrapper
        .iter()
        .any(|variant| variant.get("type").and_then(serde_json::Value::as_str) == Some("boolean"));
    let accepts_config = wrapper.iter().any(|variant| {
        variant.get("$ref").and_then(serde_json::Value::as_str)
            == Some("#/definitions/WorkflowConfigToml")
    });
    assert!(
        accepts_bool && accepts_config,
        "workflow feature should accept both a boolean and a config table"
    );

    // The `[features.workflow] enabled = true` form must be representable: the
    // config table exposes an `enabled` boolean.
    let enabled = schema_value
        .pointer("/definitions/WorkflowConfigToml/properties/enabled")
        .expect("WorkflowConfigToml should expose an `enabled` property");
    assert_eq!(
        enabled.get("type").and_then(serde_json::Value::as_str),
        Some("boolean"),
        "workflow `enabled` should be a boolean"
    );
}

#[test]
fn config_schema_hides_unsupported_inline_mcp_bearer_token() {
    let schema_json = config_schema_json().expect("serialize config schema");
    let schema_value: serde_json::Value =
        serde_json::from_slice(&schema_json).expect("decode schema json");
    let properties = schema_value
        .pointer("/definitions/RawMcpServerConfig/properties")
        .expect("RawMcpServerConfig properties should exist")
        .as_object()
        .expect("RawMcpServerConfig properties should be an object");

    assert_eq!(
        (
            properties.contains_key("bearer_token"),
            properties.contains_key("bearer_token_env_var"),
        ),
        (false, true),
    );
}

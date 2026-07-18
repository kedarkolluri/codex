//! Tests for `Feature::Workflow`'s transitive dependency validation (§13 R6).
//!
//! Two layers are covered:
//! - the [`validate_workflow_feature_dependencies`] validator in isolation, and
//! - the actual [`ConfigBuilder`] loading path that wires the validator in, so a
//!   regression that drops the validator call (rather than breaking the
//!   validator itself) is still caught.

use super::*;

use tempfile::tempdir;

fn features_toml(source: &str) -> FeaturesToml {
    toml::from_str(source).expect("valid features toml")
}

#[test]
fn workflow_with_deps_unset_is_ok() {
    let features = features_toml("workflow = true");
    validate_workflow_feature_dependencies(Some(&features), true)
        .expect("workflow with unset deps should be accepted (normalize auto-enables them)");
}

#[test]
fn workflow_with_deps_explicitly_enabled_is_ok() {
    let features = features_toml("workflow = true\ncode_mode = true\nmulti_agent_v2 = true");
    validate_workflow_feature_dependencies(Some(&features), true)
        .expect("explicitly enabled deps should be accepted");
}

#[test]
fn workflow_with_code_mode_disabled_errors_and_names_dependency() {
    let features = features_toml("workflow = true\ncode_mode = false");
    let err = validate_workflow_feature_dependencies(Some(&features), true)
        .expect_err("explicitly disabled code_mode should error");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("features.code_mode"),
        "message should name the missing dependency: {err}"
    );
}

#[test]
fn workflow_with_multi_agent_v2_disabled_errors_and_names_dependency() {
    let features = features_toml("workflow = true\nmulti_agent_v2 = false");
    let err = validate_workflow_feature_dependencies(Some(&features), true)
        .expect_err("explicitly disabled multi_agent_v2 should error");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("features.multi_agent_v2"),
        "message should name the missing dependency: {err}"
    );
}

#[test]
fn workflow_off_ignores_disabled_dependencies() {
    let features = features_toml("code_mode = false\nmulti_agent_v2 = false");
    validate_workflow_feature_dependencies(Some(&features), false)
        .expect("workflow off should not force-enable or error on the deps");
}

/// End-to-end coverage of the validator *wiring*: an actual `Config` load with
/// `features.workflow = true` and `features.code_mode` explicitly disabled must
/// surface the dependency error, not silently auto-enable the dependency. This
/// drives `ConfigBuilder::build` (not the validator directly), so a regression
/// that removes the `validate_workflow_feature_dependencies` call from the load
/// path is caught even though the validator itself still works.
#[tokio::test]
async fn config_loading_surfaces_workflow_dependency_error() {
    let tmp = tempdir().expect("tempdir");
    let contents = "[features]\nworkflow = true\ncode_mode = false\n";
    let config_path = tmp.path().join(CONFIG_TOML_FILE);
    std::fs::write(&config_path, contents).expect("write config");

    let err = ConfigBuilder::default()
        .codex_home(tmp.path().to_path_buf())
        .fallback_cwd(Some(tmp.path().to_path_buf()))
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await
        .expect_err("workflow with explicitly disabled code_mode must fail to load");

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    assert!(
        err.to_string().contains("features.code_mode"),
        "config load error should name the disabled dependency: {err}"
    );
}

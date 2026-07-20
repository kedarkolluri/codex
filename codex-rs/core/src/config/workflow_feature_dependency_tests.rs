use super::*;
use codex_config::test_support::CloudConfigBundleFixture;
use pretty_assertions::assert_eq;
use tempfile::TempDir;

use super::workflow_feature_dependencies::validate_workflow_feature_dependencies;

fn features_toml(source: &str) -> FeaturesToml {
    toml::from_str(source).expect("valid features TOML")
}

#[test]
fn workflow_dependency_validation_distinguishes_unset_and_explicit_false() {
    let unset = features_toml("workflow = true");
    validate_workflow_feature_dependencies(Some(&unset), /*workflow_enabled*/ true)
        .expect("unset workflow dependencies should be enabled during normalization");

    let enabled = features_toml("workflow = true\ncode_mode = true\nmulti_agent_v2 = true");
    validate_workflow_feature_dependencies(Some(&enabled), /*workflow_enabled*/ true)
        .expect("explicitly enabled workflow dependencies should be accepted");

    for dependency in ["code_mode", "multi_agent_v2"] {
        let features = features_toml(&format!("workflow = true\n{dependency} = false"));
        let error =
            validate_workflow_feature_dependencies(Some(&features), /*workflow_enabled*/ true)
                .expect_err("an explicitly disabled workflow dependency should fail");
        assert_eq!(
            (error.kind(), error.to_string()),
            (
                std::io::ErrorKind::InvalidInput,
                format!(
                    "`features.workflow` requires `features.{dependency}`; remove the explicit `enabled = false` override or enable `features.{dependency}`"
                )
            )
        );
    }

    let disabled = features_toml("workflow = false\ncode_mode = false\nmulti_agent_v2 = false");
    validate_workflow_feature_dependencies(Some(&disabled), /*workflow_enabled*/ false)
        .expect("disabled workflows should not constrain their dependencies");
}

#[tokio::test]
async fn config_loading_rejects_explicitly_disabled_workflow_dependencies() {
    for dependency in ["code_mode", "multi_agent_v2"] {
        let codex_home = TempDir::new().expect("create temporary Codex home");
        std::fs::write(
            codex_home.path().join(CONFIG_TOML_FILE),
            format!("[features]\nworkflow = true\n{dependency} = false\n"),
        )
        .expect("write config");

        let error = ConfigBuilder::default()
            .codex_home(codex_home.path().to_path_buf())
            .fallback_cwd(Some(codex_home.path().to_path_buf()))
            .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
            .build()
            .await
            .expect_err("config loading should reject an explicitly disabled dependency");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            error
                .to_string()
                .contains(&format!("features.{dependency}"))
        );
    }
}

#[test]
fn config_write_validation_rejects_explicitly_disabled_workflow_dependencies() {
    for dependency in ["code_mode", "multi_agent_v2"] {
        let config: ConfigToml = toml::from_str(&format!(
            "[features]\nworkflow = true\n{dependency} = false\n"
        ))
        .expect("valid config TOML");

        let error = validate_feature_requirements_for_config_toml(
            &config, /*feature_requirements*/ None,
        )
        .expect_err("config writes should reject explicitly disabled workflow dependencies");
        assert_eq!(
            (error.kind(), error.to_string()),
            (
                std::io::ErrorKind::InvalidInput,
                format!(
                    "`features.workflow` requires `features.{dependency}`; remove the explicit `enabled = false` override or enable `features.{dependency}`"
                )
            )
        );
    }
}

#[tokio::test]
async fn managed_disabled_workflow_does_not_enable_its_dependencies() {
    let codex_home = TempDir::new().expect("create temporary Codex home");
    std::fs::write(
        codex_home.path().join(CONFIG_TOML_FILE),
        "[features]\nworkflow = true\ncode_mode = false\nmulti_agent_v2 = false\n",
    )
    .expect("write config");

    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(codex_home.path().to_path_buf())
        .fallback_cwd(Some(codex_home.path().to_path_buf()))
        .cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                "[features]\nworkflow = false\n",
            ),
        )
        .build()
        .await
        .expect("managed-disabled workflow config should load");

    assert_eq!(
        (
            config.features.enabled(Feature::Workflow),
            config.features.enabled(Feature::CodeMode),
            config.features.enabled(Feature::MultiAgentV2),
        ),
        (false, false, false)
    );
}

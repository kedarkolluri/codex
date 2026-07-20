use super::*;
use crate::agent::role::apply_role_to_config as apply_ordinary_role_to_config;
use crate::agent::role::apply_workflow_role_to_config as apply_role_to_config;
use crate::config::AgentRoleConfig;
use crate::config::Config;
use crate::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use toml::Value as TomlValue;

const CUSTOM_ROLE: &str = "bounded-role";

async fn test_config_with_cli_overrides(
    cli_overrides: Vec<(String, TomlValue)>,
) -> (TempDir, Config) {
    let home = TempDir::new().expect("create temp dir");
    let home_path = home.path().to_path_buf();
    let config = ConfigBuilder::default()
        .codex_home(home_path.clone())
        .cli_overrides(cli_overrides)
        .fallback_cwd(Some(home_path))
        .build()
        .await
        .expect("load test config");
    (home, config)
}

async fn install_role_file(config: &mut Config, home: &TempDir, contents: &str) {
    let role_path = home.path().join("bounded-role.toml");
    tokio::fs::write(&role_path, contents)
        .await
        .expect("write role config");
    config.agent_roles.insert(
        CUSTOM_ROLE.to_string(),
        AgentRoleConfig {
            description: Some("Bounded test role".to_string()),
            config_file: Some(role_path),
            nickname_candidates: None,
        },
    );
}

fn toml_string(value: &str) -> String {
    TomlValue::String(value.to_string()).to_string()
}

async fn assert_role_rejected_atomically(config: &mut Config) {
    let before = config.clone();

    let error = apply_role_to_config(config, Some(CUSTOM_ROLE))
        .await
        .expect_err("oversized role context should be rejected");

    assert_eq!(error, "agent type is currently not available");
    assert_eq!(config, &before);
}

#[tokio::test]
async fn role_context_accepts_exact_lane_and_combined_byte_limits() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let base_instructions = "b".repeat(MAX_ROLE_CONTEXT_LANE_BYTES);
    let compact_prompt = "c".repeat(MAX_ROLE_CONTEXT_LANE_BYTES);
    install_role_file(
        &mut config,
        &home,
        &format!(
            "instructions = {}\ncompact_prompt = {}\n",
            toml_string(&base_instructions),
            toml_string(&compact_prompt)
        ),
    )
    .await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("exact base and combined limits should be accepted");

    assert_eq!(
        config.base_instructions.as_deref(),
        Some(base_instructions.as_str())
    );
    assert_eq!(
        config.compact_prompt.as_deref(),
        Some(compact_prompt.as_str())
    );

    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let developer_instructions = "d".repeat(MAX_ROLE_CONTEXT_LANE_BYTES);
    install_role_file(
        &mut config,
        &home,
        &format!(
            "developer_instructions = {}\n",
            toml_string(&developer_instructions)
        ),
    )
    .await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("exact developer-instruction limit should be accepted");

    assert_eq!(
        config.developer_instructions.as_deref(),
        Some(developer_instructions.as_str())
    );
}

#[tokio::test]
async fn role_context_enforces_the_utf8_byte_boundary_atomically() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let exact_developer_instructions = format!(
        "{}é",
        "d".repeat(MAX_ROLE_CONTEXT_LANE_BYTES.saturating_sub(2))
    );
    assert_eq!(
        exact_developer_instructions.len(),
        MAX_ROLE_CONTEXT_LANE_BYTES
    );
    install_role_file(
        &mut config,
        &home,
        &format!(
            "developer_instructions = {}\n",
            toml_string(&exact_developer_instructions)
        ),
    )
    .await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("an exact multibyte boundary should be accepted");
    assert_eq!(
        config.developer_instructions.as_deref(),
        Some(exact_developer_instructions.as_str())
    );

    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let oversized_developer_instructions = format!(
        "{}é",
        "d".repeat(MAX_ROLE_CONTEXT_LANE_BYTES.saturating_sub(1))
    );
    assert_eq!(
        oversized_developer_instructions.len(),
        MAX_ROLE_CONTEXT_LANE_BYTES + 1
    );
    install_role_file(
        &mut config,
        &home,
        &format!(
            "developer_instructions = {}\n",
            toml_string(&oversized_developer_instructions)
        ),
    )
    .await;

    assert_role_rejected_atomically(&mut config).await;
}

#[tokio::test]
async fn role_context_rejects_combined_overflow_atomically() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let base_instructions = "b".repeat(3_000);
    let developer_instructions = "d".repeat(3_000);
    let compact_prompt = "c".repeat(2_001);
    install_role_file(
        &mut config,
        &home,
        &format!(
            "instructions = {}\ndeveloper_instructions = {}\ncompact_prompt = {}\n",
            toml_string(&base_instructions),
            toml_string(&developer_instructions),
            toml_string(&compact_prompt)
        ),
    )
    .await;

    assert_role_rejected_atomically(&mut config).await;
}

#[tokio::test]
async fn role_context_bounds_resolved_instruction_files_atomically() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let exact_base_instructions = "b".repeat(MAX_ROLE_CONTEXT_LANE_BYTES);
    tokio::fs::write(
        home.path().join("base-instructions.txt"),
        &exact_base_instructions,
    )
    .await
    .expect("write base instructions");
    install_role_file(
        &mut config,
        &home,
        "model_instructions_file = \"base-instructions.txt\"\n",
    )
    .await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("an exact file-backed base-instruction limit should be accepted");
    assert_eq!(
        config.base_instructions.as_deref(),
        Some(exact_base_instructions.as_str())
    );

    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let exact_compact_prompt = "c".repeat(MAX_ROLE_CONTEXT_LANE_BYTES);
    tokio::fs::write(
        home.path().join("compact-prompt.txt"),
        &exact_compact_prompt,
    )
    .await
    .expect("write compact prompt");
    install_role_file(
        &mut config,
        &home,
        "experimental_compact_prompt_file = \"compact-prompt.txt\"\n",
    )
    .await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("an exact file-backed compact-prompt limit should be accepted");
    assert_eq!(
        config.compact_prompt.as_deref(),
        Some(exact_compact_prompt.as_str())
    );

    for (config_key, file_name) in [
        ("model_instructions_file", "base-instructions.txt"),
        ("experimental_compact_prompt_file", "compact-prompt.txt"),
    ] {
        let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
        tokio::fs::write(
            home.path().join(file_name),
            "x".repeat(MAX_ROLE_CONTEXT_LANE_BYTES + 1),
        )
        .await
        .expect("write oversized instructions");
        install_role_file(
            &mut config,
            &home,
            &format!("{config_key} = {file_name:?}\n"),
        )
        .await;

        assert_role_rejected_atomically(&mut config).await;
    }
}

#[tokio::test]
async fn role_context_bounds_project_docs_override_atomically() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    install_role_file(
        &mut config,
        &home,
        &format!("project_doc_max_bytes = {MAX_ROLE_PROJECT_DOC_BYTES}\n"),
    )
    .await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("exact project-doc limit should be accepted");
    assert_eq!(config.project_doc_max_bytes, MAX_ROLE_PROJECT_DOC_BYTES);

    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    install_role_file(
        &mut config,
        &home,
        &format!(
            "project_doc_max_bytes = {}\n",
            MAX_ROLE_PROJECT_DOC_BYTES + 1
        ),
    )
    .await;

    assert_role_rejected_atomically(&mut config).await;
}

#[tokio::test]
async fn ordinary_role_application_does_not_apply_workflow_bounds() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let developer_instructions = "d".repeat(MAX_ROLE_CONTEXT_LANE_BYTES + 1);
    install_role_file(
        &mut config,
        &home,
        &format!(
            "developer_instructions = {}\n",
            toml_string(&developer_instructions)
        ),
    )
    .await;

    apply_ordinary_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("workflow-only context bounds must not change ordinary roles");

    assert_eq!(
        config.developer_instructions.as_deref(),
        Some(developer_instructions.as_str())
    );
}

#[tokio::test]
async fn role_context_does_not_validate_lanes_the_role_did_not_override() {
    enum InheritedLane {
        Base,
        Developer,
        Compact,
    }

    for (config_key, lane) in [
        ("instructions", InheritedLane::Base),
        ("developer_instructions", InheritedLane::Developer),
        ("compact_prompt", InheritedLane::Compact),
    ] {
        let inherited = "i".repeat(MAX_ROLE_CONTEXT_LANE_BYTES + 1);
        let (home, mut config) = test_config_with_cli_overrides(vec![(
            config_key.to_string(),
            TomlValue::String(inherited.clone()),
        )])
        .await;
        install_role_file(&mut config, &home, "model = \"role-model\"\n").await;

        apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
            .await
            .expect("an inherited oversized lane should not reject an unrelated role override");

        let effective = match lane {
            InheritedLane::Base => config.base_instructions.as_deref(),
            InheritedLane::Developer => config.developer_instructions.as_deref(),
            InheritedLane::Compact => config.compact_prompt.as_deref(),
        };
        assert_eq!(effective, Some(inherited.as_str()));
        assert_eq!(config.model.as_deref(), Some("role-model"));
    }

    let inherited_project_doc_bytes = MAX_ROLE_PROJECT_DOC_BYTES + 1;
    let (home, mut config) = test_config_with_cli_overrides(vec![(
        "project_doc_max_bytes".to_string(),
        TomlValue::Integer(
            i64::try_from(inherited_project_doc_bytes).expect("project-doc limit fits in i64"),
        ),
    )])
    .await;
    install_role_file(&mut config, &home, "model = \"role-model\"\n").await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("an inherited project-doc limit should not reject an unrelated role override");

    assert_eq!(config.project_doc_max_bytes, inherited_project_doc_bytes);
    assert_eq!(config.model.as_deref(), Some("role-model"));
}

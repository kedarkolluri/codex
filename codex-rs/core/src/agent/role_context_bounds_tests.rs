use super::*;
use crate::agent::role::apply_role_to_config as apply_ordinary_role_to_config;
use crate::agent::role::apply_workflow_role_to_config as apply_role_to_config;
use crate::agent::role::spawn_tool_spec;
use crate::config::AgentRoleConfig;
use crate::config::Config;
use crate::config::ConfigBuilder;
use pretty_assertions::assert_eq;
use std::collections::BTreeMap;
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
async fn role_context_rejects_utf8_lane_overflow_without_echo_or_mutation() {
    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let developer_instructions = "é".repeat(MAX_ROLE_CONTEXT_LANE_BYTES / 2 + 1);
    install_role_file(
        &mut config,
        &home,
        &format!(
            "developer_instructions = {}\n",
            toml_string(&developer_instructions)
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
    tokio::fs::write(
        home.path().join("base-instructions.txt"),
        "b".repeat(MAX_ROLE_CONTEXT_LANE_BYTES + 1),
    )
    .await
    .expect("write base instructions");
    install_role_file(
        &mut config,
        &home,
        "model_instructions_file = \"base-instructions.txt\"\n",
    )
    .await;

    assert_role_rejected_atomically(&mut config).await;

    let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    tokio::fs::write(
        home.path().join("compact-prompt.txt"),
        "c".repeat(MAX_ROLE_CONTEXT_LANE_BYTES + 1),
    )
    .await
    .expect("write compact prompt");
    install_role_file(
        &mut config,
        &home,
        "experimental_compact_prompt_file = \"compact-prompt.txt\"\n",
    )
    .await;

    assert_role_rejected_atomically(&mut config).await;
}

#[tokio::test]
async fn role_context_rejects_legacy_profile_file_indirection_atomically() {
    for profile_field in [
        "model_instructions_file",
        "experimental_compact_prompt_file",
    ] {
        let (home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
        tokio::fs::write(
            home.path().join("oversized.txt"),
            "x".repeat(MAX_ROLE_CONTEXT_LANE_BYTES + 1),
        )
        .await
        .expect("write oversized profile context");
        install_role_file(
            &mut config,
            &home,
            &format!(
                "profile = \"oversized\"\n[profiles.oversized]\n{profile_field} = \"oversized.txt\"\n"
            ),
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
async fn workflow_child_bounds_inherited_project_docs_without_rejecting_role() {
    let inherited_limit = MAX_ROLE_PROJECT_DOC_BYTES + 1;
    let (home, mut config) = test_config_with_cli_overrides(vec![(
        "project_doc_max_bytes".to_string(),
        TomlValue::Integer(i64::try_from(inherited_limit).expect("limit fits in i64")),
    )])
    .await;
    install_role_file(&mut config, &home, "model = \"role-model\"\n").await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("an inherited oversized project-doc limit should not reject the role");
    assert_eq!(config.project_doc_max_bytes, inherited_limit);

    bound_workflow_child_context(&mut config)
        .expect("inherited project docs should be clamped");
    assert_eq!(config.project_doc_max_bytes, MAX_ROLE_PROJECT_DOC_BYTES);
}

#[tokio::test]
async fn workflow_child_clamps_inherited_tool_output_limit() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    config.tool_output_token_limit = Some(usize::MAX);

    bound_workflow_child_context(&mut config).expect("default child context should be valid");

    assert_eq!(
        config.tool_output_token_limit,
        Some(crate::context::MAX_WORKFLOW_CHILD_TOOL_OUTPUT_TOKENS)
    );
}

#[tokio::test]
async fn workflow_child_rejects_oversized_inherited_instruction_lanes() {
    for field in [
        "instructions",
        "developer_instructions",
        "compact_prompt",
    ] {
        let oversized = "x".repeat(MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES + 1);
        let (home, mut config) = test_config_with_cli_overrides(vec![(
            field.to_string(),
            TomlValue::String(oversized),
        )])
        .await;
        install_role_file(&mut config, &home, "model = \"role-model\"\n").await;

        apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
            .await
            .expect("an unrelated role override should preserve inherited context");

        let error = bound_workflow_child_context(&mut config)
            .expect_err("workflow child context above the per-item cap should be rejected");
        assert_eq!(
            error.to_string(),
            match field {
                "instructions" => format!(
                    "workflow child base instructions exceed the {MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES}-byte UTF-8 limit and do not match audited product context (got {} bytes)",
                    MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES + 1
                ),
                "developer_instructions" | "compact_prompt" => format!(
                    "workflow child {} exceeds the {MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES}-byte UTF-8 limit (got {} bytes)",
                    match field {
                        "developer_instructions" => "developer instructions",
                        "compact_prompt" => "compact prompt",
                        other => panic!("unexpected field {other}"),
                    },
                    MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES + 1
                ),
                other => panic!("unexpected field {other}"),
            }
        );
    }
}

#[tokio::test]
async fn workflow_child_accepts_custom_instruction_lanes_at_byte_limit() {
    let exact = "x".repeat(MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES);
    let (home, mut config) = test_config_with_cli_overrides(vec![
        (
            "instructions".to_string(),
            TomlValue::String(exact.clone()),
        ),
        (
            "developer_instructions".to_string(),
            TomlValue::String(exact.clone()),
        ),
        ("compact_prompt".to_string(), TomlValue::String(exact)),
    ])
    .await;
    install_role_file(&mut config, &home, "model = \"role-model\"\n").await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("an unrelated role override should preserve inherited context");

    bound_workflow_child_context(&mut config)
        .expect("workflow child context at the per-item cap should be accepted");
}

#[tokio::test]
async fn workflow_child_accepts_audited_product_base_instructions() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let model = codex_models_manager::bundled_models_response()
        .expect("bundled models should parse")
        .models
        .into_iter()
        .find(|model| model.slug == "gpt-5.2")
        .expect("gpt-5.2 should be bundled");
    assert!(model.base_instructions.len() > MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES);
    config.base_instructions = Some(model.base_instructions);

    bound_workflow_child_context(&mut config)
        .expect("audited product instructions should retain their full text");
}

#[tokio::test]
async fn workflow_child_rejects_high_token_unicode_under_the_model_byte_backstop() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let instructions = "\u{10ffff}".repeat(MAX_WORKFLOW_CHILD_MODEL_BASE_BYTES / 4);
    assert_eq!(instructions.len(), MAX_WORKFLOW_CHILD_MODEL_BASE_BYTES);
    config.base_instructions = Some(instructions);

    let error = bound_workflow_child_context(&mut config)
        .expect_err("a high-token string must not gain trust from the 32 KiB backstop");

    assert_eq!(
        error.to_string(),
        format!(
            "workflow child base instructions exceed the {MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES}-byte UTF-8 limit and do not match audited product context (got {MAX_WORKFLOW_CHILD_MODEL_BASE_BYTES} bytes)"
        )
    );
}

#[tokio::test]
async fn workflow_child_rejects_unknown_remote_model_instructions() {
    let (_home, mut config) = test_config_with_cli_overrides(Vec::new()).await;
    let instructions = "remote model metadata\n"
        .repeat(MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES / 22 + 1);
    assert!(instructions.len() > MAX_WORKFLOW_CHILD_CUSTOM_CONTEXT_LANE_BYTES);
    config.model = Some("gpt-5.2".to_string());
    config.base_instructions = Some(instructions);

    bound_workflow_child_context(&mut config)
        .expect_err("a known model slug must not authenticate unknown remote instructions");
}

#[tokio::test]
async fn ordinary_role_preserves_preexisting_large_context_overrides() {
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
        .expect("workflow-only context bounds must not break ordinary roles");

    assert_eq!(
        config.developer_instructions.as_deref(),
        Some(developer_instructions.as_str())
    );
}

#[tokio::test]
async fn role_context_does_not_validate_lanes_the_role_did_not_override() {
    let inherited_base_instructions = "i".repeat(MAX_ROLE_CONTEXT_LANE_BYTES + 1);
    let (home, mut config) = test_config_with_cli_overrides(vec![(
        "instructions".to_string(),
        TomlValue::String(inherited_base_instructions.clone()),
    )])
    .await;
    install_role_file(&mut config, &home, "model = \"role-model\"\n").await;

    apply_role_to_config(&mut config, Some(CUSTOM_ROLE))
        .await
        .expect("an inherited oversized lane should not reject an unrelated role override");

    assert_eq!(
        config.base_instructions.as_deref(),
        Some(inherited_base_instructions.as_str())
    );
    assert_eq!(config.model.as_deref(), Some("role-model"));
}

#[test]
fn role_catalog_bounds_entries_count_and_total_bytes_utf8_safely() {
    let oversized_entry = format!("unicode: {}", "🦀".repeat(MAX_ROLE_CATALOG_ENTRY_BYTES));
    let bounded_entry = truncate_catalog_entry(oversized_entry);
    assert!(bounded_entry.len() <= MAX_ROLE_CATALOG_ENTRY_BYTES);
    assert!(bounded_entry.ends_with(ROLE_CATALOG_ENTRY_OMISSION_MARKER));

    let entries = (0..40).map(|index| format!("entry-{index:02}"));
    let catalog = bound_role_catalog("roles:", entries);
    assert!(catalog.len() <= MAX_ROLE_CATALOG_BYTES);
    assert_eq!(
        catalog
            .lines()
            .filter(|line| line.starts_with("entry-"))
            .count(),
        MAX_ROLE_CATALOG_ENTRIES
    );
    assert!(catalog.contains("entry-31"));
    assert!(!catalog.contains("entry-32"));
    assert!(catalog.ends_with(ROLE_CATALOG_OMISSION_MARKER));

    let catalog = bound_role_catalog(
        "roles:",
        (0..4).map(|index| format!("entry-{index}: {}", "x".repeat(1_980))),
    );
    assert!(catalog.len() <= MAX_ROLE_CATALOG_BYTES);
    assert!(catalog.ends_with(ROLE_CATALOG_OMISSION_MARKER));
}

#[test]
fn spawn_tool_role_catalog_applies_the_shared_bounds() {
    let user_defined_roles = (0..40)
        .map(|index| {
            (
                format!("role-{index:02}"),
                AgentRoleConfig {
                    description: Some(format!("role description {index}")),
                    config_file: None,
                    nickname_candidates: None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();

    let catalog = spawn_tool_spec::build(&user_defined_roles);

    assert!(catalog.len() <= MAX_ROLE_CATALOG_BYTES);
    assert!(catalog.ends_with(ROLE_CATALOG_OMISSION_MARKER));
}

#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used)]

use anyhow::Result;
use codex_core::config::AgentRoleConfig;
use codex_core::config::Config;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelServiceTier;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::default_input_modalities;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_models_once;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::namespace_child_tool;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use std::time::Duration;
use std::time::Instant;
use test_case::test_case;
use tokio::time::sleep;

const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";
const SPAWN_AGENT_TOOL_NAME: &str = "spawn_agent";

fn max_workflow_usage_hint() -> String {
    format!("WORKFLOW_USAGE_HINT:{}", "u".repeat(980))
}

fn configure_workflow_prompt_and_roles(config: &mut Config, usage_hint_text: String) {
    for feature in [
        Feature::Collab,
        Feature::MultiAgentV2,
        Feature::CodeMode,
        Feature::Workflow,
    ] {
        config
            .features
            .enable(feature)
            .expect("test config should allow feature update");
    }
    config.multi_agent_v2.non_code_mode_only = false;
    config.multi_agent_v2.usage_hint_text = Some(usage_hint_text);
    for index in 0..40 {
        let description = if index == 0 {
            format!("role description {index}: {}", "🦀".repeat(1_000))
        } else {
            (0..80)
                .map(|line| format!("role {index} line {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        };
        config.agent_roles.insert(
            format!("role-{index:02}"),
            AgentRoleConfig {
                description: Some(description),
                config_file: None,
                nickname_candidates: None,
            },
        );
    }
}

fn spawn_agent_description(body: &Value) -> Option<String> {
    namespace_child_tool(body, MULTI_AGENT_V1_NAMESPACE, SPAWN_AGENT_TOOL_NAME)
        .and_then(|tool| tool.get("description"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn spawn_agent_exposes_agent_type(body: &Value, namespace: &str) -> bool {
    namespace_child_tool(body, namespace, SPAWN_AGENT_TOOL_NAME)
        .and_then(|tool| tool.pointer("/parameters/properties/agent_type"))
        .is_some()
}

fn test_model_info(
    slug: &str,
    display_name: &str,
    description: &str,
    visibility: ModelVisibility,
    default_reasoning_level: ReasoningEffort,
    supported_reasoning_levels: Vec<ReasoningEffortPreset>,
    service_tiers: Vec<ModelServiceTier>,
) -> ModelInfo {
    ModelInfo {
        slug: slug.to_string(),
        display_name: display_name.to_string(),
        description: Some(description.to_string()),
        default_reasoning_level: Some(default_reasoning_level),
        supported_reasoning_levels,
        shell_type: ConfigShellToolType::ShellCommand,
        visibility,
        supported_in_api: true,
        input_modalities: default_input_modalities(),
        used_fallback_model_metadata: false,
        supports_search_tool: false,
        use_responses_lite: false,
        auto_review_model_override: None,
        tool_mode: None,
        multi_agent_version: None,
        priority: 1,
        additional_speed_tiers: Vec::new(),
        service_tiers,
        default_service_tier: None,
        upgrade: None,
        base_instructions: "base instructions".to_string(),
        model_messages: None,
        include_skills_usage_instructions: false,
        supports_reasoning_summary_parameter: true,
        default_reasoning_summary: ReasoningSummary::Auto,
        support_verbosity: false,
        default_verbosity: None,
        availability_nux: None,
        apply_patch_tool_type: None,
        web_search_tool_type: Default::default(),
        truncation_policy: TruncationPolicyConfig::bytes(/*limit*/ 10_000),
        supports_parallel_tool_calls: false,
        supports_image_detail_original: false,
        context_window: Some(272_000),
        max_context_window: None,
        auto_compact_token_limit: None,
        comp_hash: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
    }
}

async fn wait_for_model_available(manager: &SharedModelsManager, slug: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let available_models = manager
            .list_models(
                RefreshStrategy::Online,
                codex_core::test_support::default_http_client_factory(),
            )
            .await;
        if available_models.iter().any(|model| model.model == slug) {
            return;
        }
        if Instant::now() >= deadline {
            panic!("timed out waiting for remote model {slug} to appear");
        }
        sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_description_lists_visible_models_and_reasoning_efforts() -> Result<()> {
    let server = start_mock_server().await;
    mount_models_once(
        &server,
        ModelsResponse {
            models: vec![
                test_model_info(
                    "visible-model",
                    "Visible Model",
                    "Fast and capable",
                    ModelVisibility::List,
                    ReasoningEffort::Medium,
                    vec![
                        ReasoningEffortPreset {
                            effort: ReasoningEffort::Low,
                            description: "Quick scan".to_string(),
                        },
                        ReasoningEffortPreset {
                            effort: ReasoningEffort::Medium,
                            description: "Balanced".to_string(),
                        },
                        ReasoningEffortPreset {
                            effort: ReasoningEffort::High,
                            description: "Deep dive".to_string(),
                        },
                    ],
                    vec![ModelServiceTier {
                        id: "priority".to_string(),
                        name: "Fast".to_string(),
                        description: "1.5x speed, increased usage".to_string(),
                    }],
                ),
                test_model_info(
                    "hidden-model",
                    "Hidden Model",
                    "Should not be shown",
                    ModelVisibility::Hide,
                    ReasoningEffort::Low,
                    vec![ReasoningEffortPreset {
                        effort: ReasoningEffort::Low,
                        description: "Not visible".to_string(),
                    }],
                    Vec::new(),
                ),
            ],
        },
    )
    .await;
    let resp_mock = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;

    let mut builder = test_codex()
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_model("visible-model")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config.multi_agent_v2.hide_spawn_agent_metadata = false;
        });
    let test = builder.build(&server).await?;
    wait_for_model_available(&test.thread_manager.get_models_manager(), "visible-model").await;

    test.submit_turn("hello").await?;

    let body = resp_mock.single_request().body_json();
    let description =
        spawn_agent_description(&body).expect("spawn_agent description should be present");

    assert!(
        description.contains("- `visible-model`: Fast and capable"),
        "expected visible model summary in spawn_agent description: {description:?}"
    );
    assert!(
        description
            .contains("Available model overrides (optional; inherited parent model is preferred):"),
        "expected model choices to be framed as overrides in spawn_agent description: {description:?}"
    );
    assert!(
        description.contains(
            "Spawned agents inherit your current model by default. Omit `model` to use that preferred default; set `model` only when an explicit override is needed."
        ),
        "expected inherited-model guidance in spawn_agent description: {description:?}"
    );
    assert!(
        description.contains(
            "Do not set the `model` field unless the user explicitly asks for a different model or there is a clear task-specific reason."
        ),
        "expected model override usage guidance in spawn_agent description: {description:?}"
    );
    assert!(
        description.contains("Reasoning efforts: low, medium (default), high."),
        "expected default reasoning effort in spawn_agent description: {description:?}"
    );
    assert!(
        description.contains("Service tiers: priority."),
        "expected service tier guidance in spawn_agent description: {description:?}"
    );
    assert!(
        !description.contains("hidden-model"),
        "hidden picker model should be omitted from spawn_agent description: {description:?}"
    );
    assert!(
        description.contains(
            "Do not spawn sub-agents unless the user or applicable AGENTS.md/skill instructions explicitly ask for sub-agents, delegation, or parallel agent work."
        ),
        "expected explicit authorization rule in spawn_agent description: {description:?}"
    );
    assert!(
        description.contains(
            "Requests for depth, thoroughness, research, investigation, or detailed codebase analysis do not count as permission to spawn."
        ) && description.contains("### When to delegate vs. do the subtask yourself"),
        "expected delegation decision guidance in spawn_agent description: {description:?}"
    );
    assert!(
        description.contains(
            "Agent-role guidance below only helps choose which agent to use after spawning is already authorized; it never authorizes spawning by itself."
        ),
        "expected agent-role clarification in spawn_agent description: {description:?}"
    );
    assert!(
        !description.contains("A mini model can solve many tasks faster than the main model."),
        "spawn_agent description should not encourage choosing a smaller model by default: {description:?}"
    );

    Ok(())
}

#[test_case(false, false, MULTI_AGENT_V1_NAMESPACE; "v1 hides agent type without roles")]
#[test_case(true, true, "collaboration"; "v2 exposes agent type with a role")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_agent_roles_control_spawn_agent_type(
    multi_agent_v2: bool,
    has_agent_role: bool,
    namespace: &str,
) -> Result<()> {
    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;
    let test = test_codex()
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            if multi_agent_v2 {
                config
                    .features
                    .enable(Feature::MultiAgentV2)
                    .expect("test config should allow feature update");
            } else {
                config
                    .features
                    .disable(Feature::MultiAgentV2)
                    .expect("test config should allow feature update");
            }
            if has_agent_role {
                config.agent_roles.insert(
                    "researcher".to_string(),
                    AgentRoleConfig {
                        description: Some("Research role".to_string()),
                        config_file: None,
                        nickname_candidates: None,
                    },
                );
            }
        })
        .build_with_auto_env(&server)
        .await?;

    test.submit_turn("hello").await?;

    assert_eq!(
        spawn_agent_exposes_agent_type(&response.single_request().body_json(), namespace),
        has_agent_role
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_prompt_and_role_catalog_are_bounded_in_code_mode_request() -> Result<()> {
    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;
    let usage_hint_text = max_workflow_usage_hint();
    assert_eq!(usage_hint_text.len(), 1_000);
    let usage_hint_for_config = usage_hint_text.clone();
    let test = test_codex()
        .with_config(move |config| {
            configure_workflow_prompt_and_roles(config, usage_hint_for_config);
        })
        .build_with_auto_env(&server)
        .await?;

    test.submit_turn("hello").await?;

    let body = response.single_request().body_json();
    let spawn_agent = namespace_child_tool(&body, "collaboration", SPAWN_AGENT_TOOL_NAME)
        .expect("spawn_agent should be present");
    let description = spawn_agent
        .pointer("/parameters/properties/agent_type/description")
        .and_then(Value::as_str)
        .expect("spawn_agent agent_type description should be present");
    let catalog_start = description
        .find("Available roles:")
        .expect("agent_type description should contain the role catalog");
    let catalog = &description[catalog_start..];
    let catalog_lines = catalog
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    assert!(catalog.len() <= 4_000);
    assert!(catalog_lines.len() <= 64);
    assert!(catalog.contains("role-00"));
    assert!(catalog.contains("... [entry truncated]"));
    assert!(!catalog.contains("role-01"));
    assert!(catalog.ends_with("... [additional roles omitted]"));

    let rendered_catalog = catalog_lines
        .iter()
        .map(|line| format!("  // {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    let augmented_description = spawn_agent
        .get("description")
        .and_then(Value::as_str)
        .expect("Code Mode spawn_agent description should be present");
    assert!(augmented_description.contains("exec tool declaration:"));
    assert!(augmented_description.contains(&rendered_catalog));
    assert_eq!(augmented_description.matches(&usage_hint_text).count(), 1);
    assert!(catalog.len() + rendered_catalog.len() <= 8_320);
    assert!(catalog.len() + rendered_catalog.len() + usage_hint_text.len() <= 9_320);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_prompt_and_rendered_role_catalog_are_bounded_in_code_mode_only_request()
-> Result<()> {
    let server = start_mock_server().await;
    let response = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp1"), ev_completed("resp1")]),
    )
    .await;
    let usage_hint_text = max_workflow_usage_hint();
    assert_eq!(usage_hint_text.len(), 1_000);
    let usage_hint_for_config = usage_hint_text.clone();
    let test = test_codex()
        .with_config(move |config| {
            configure_workflow_prompt_and_roles(config, usage_hint_for_config);
            config
                .features
                .enable(Feature::CodeModeOnly)
                .expect("test config should allow feature update");
        })
        .build_with_auto_env(&server)
        .await?;

    test.submit_turn("hello").await?;

    let body = response.single_request().body_json();
    assert!(namespace_child_tool(&body, "collaboration", SPAWN_AGENT_TOOL_NAME).is_none());

    let exec_description = body
        .get("tools")
        .and_then(Value::as_array)
        .and_then(|tools| {
            tools.iter().find(|tool| {
                tool.get("name").and_then(Value::as_str) == Some(codex_code_mode::PUBLIC_TOOL_NAME)
            })
        })
        .and_then(|tool| tool.get("description"))
        .and_then(Value::as_str)
        .expect("Code Mode exec description should be present");
    let rendered_start = exec_description
        .find("  // Available roles:")
        .expect("exec description should contain the rendered role catalog");
    let rendered_end = exec_description[rendered_start..]
        .find("\n  agent_type")
        .map(|offset| rendered_start + offset)
        .expect("rendered role catalog should precede the agent_type declaration");
    let rendered_catalog = &exec_description[rendered_start..rendered_end];
    assert!(rendered_catalog.len() <= 4_320);
    assert!(rendered_catalog.lines().count() <= 64);
    assert!(rendered_catalog.contains("role-00"));
    assert!(rendered_catalog.contains("... [entry truncated]"));
    assert!(!rendered_catalog.contains("role-01"));
    assert!(rendered_catalog.contains("... [additional roles omitted]"));
    assert_eq!(exec_description.matches(&usage_hint_text).count(), 1);
    assert!(rendered_catalog.len() + usage_hint_text.len() <= 5_320);

    Ok(())
}

#![allow(clippy::expect_used)]

use std::io::Cursor;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_core::config::AgentRoleConfig;
use codex_exec_server::LOCAL_ENVIRONMENT_ID;
use codex_exec_server::REMOTE_ENVIRONMENT_ID;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::WorkflowEvent;
use codex_utils_path_uri::PathUri;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::skip_if_no_network;
use core_test_support::skip_if_no_remote_env;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;

const WORKFLOW_CALL_ID: &str = "workflow-role-bounds";
const WORKFLOW_NAME: &str = "role-bounds";
const CHILD_PROMPT_MARKER: &str = "ROLE_BOUNDS_CHILD_MUST_NOT_RUN";
const INHERITED_CONTEXT_CHILD_PROMPT_MARKER: &str = "INHERITED_CONTEXT_CHILD_MUST_NOT_RUN";
const ACCEPTED_CONTEXT_CHILD_PROMPT_MARKER: &str = "ACCEPTED_CONTEXT_CHILD_MUST_RUN";
const OVERSIZED_SKILL_NAME: &str = "oversized-workflow-skill";
const PROJECT_DOC_CHILD_PROMPT_MARKER: &str = "BOUNDED_PROJECT_DOC_CHILD";
const WORKFLOW_PROJECT_DOC_MAX_BYTES: usize = 8 * 1024;
const WORKFLOW_CUSTOM_CONTEXT_MAX_BYTES: usize = 8 * 1024;
const WORKFLOW_TOOL_SPEC_MAX_BYTES: usize = 8 * 1024;
const WORKFLOW_TOOL_SPECS_MAX_BYTES: usize = 64 * 1024;
const WORKFLOW_TOOL_SPECS_MAX_COUNT: usize = 64;

fn request_body_contains(request: &wiremock::Request, text: &str) -> bool {
    let compressed = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|part| part.trim() == "zstd"));
    let body = if compressed {
        zstd::stream::decode_all(Cursor::new(request.body.as_slice())).ok()
    } else {
        Some(request.body.clone())
    };
    body.and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn request_is_subagent(request: &wiremock::Request) -> bool {
    request.headers.get("x-openai-subagent").is_some()
}

fn bounded_request_tool_names(
    request: &core_test_support::responses::ResponsesRequest,
) -> Vec<String> {
    let body = request.body_json();
    let tools = body["tools"].as_array().expect("child tools array");
    assert!(tools.len() <= WORKFLOW_TOOL_SPECS_MAX_COUNT);
    assert!(
        serde_json::to_vec(tools)
            .expect("child tools should serialize")
            .len()
            <= WORKFLOW_TOOL_SPECS_MAX_BYTES
    );

    let mut logical_count = 0usize;
    let mut names = Vec::new();
    for tool in tools {
        if let Some(namespace_tools) = tool.get("tools").and_then(Value::as_array) {
            for namespace_tool in namespace_tools {
                assert!(
                    serde_json::to_vec(namespace_tool)
                        .expect("namespace tool should serialize")
                        .len()
                        <= WORKFLOW_TOOL_SPEC_MAX_BYTES
                );
                logical_count += 1;
                if let Some(name) = namespace_tool.get("name").and_then(Value::as_str) {
                    names.push(name.to_string());
                }
            }
        } else {
            assert!(
                serde_json::to_vec(tool)
                    .expect("tool should serialize")
                    .len()
                    <= WORKFLOW_TOOL_SPEC_MAX_BYTES
            );
            logical_count += 1;
            if let Some(name) = tool
                .get("name")
                .or_else(|| tool.get("type"))
                .and_then(Value::as_str)
            {
                names.push(name.to_string());
            }
        }
    }
    assert!(logical_count <= WORKFLOW_TOOL_SPECS_MAX_COUNT);
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_workflow_agent_role_rejects_before_a_child_provider_request() -> Result<()> {
    let server = responses::start_mock_server().await;
    let workflow_source = format!(
        r#"export const meta = {{ name: '{WORKFLOW_NAME}', description: 'role bound' }};
const result = await agent('{CHILD_PROMPT_MARKER}', {{ agentType: 'oversized' }});
log('ROLE_BOUNDS_RESULT:' + JSON.stringify(result));
text(JSON.stringify(result));
"#
    );
    let workflow_args = json!({"name": WORKFLOW_NAME}).to_string();
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-parent-open"),
                ev_function_call(WORKFLOW_CALL_ID, "workflow_run", &workflow_args),
                ev_completed("resp-parent-open"),
            ]),
            sse(vec![
                ev_response_created("resp-parent-final"),
                ev_assistant_message("msg-parent-final", "workflow admitted"),
                ev_completed("resp-parent-final"),
            ]),
        ],
    )
    .await;

    let server_uri = server.uri();
    let source_for_home = workflow_source.clone();
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            std::fs::write(
                home.join("config.toml"),
                format!("openai_base_url = \"{server_uri}/v1\"\n"),
            )
            .expect("write mock provider config");
            let workflows = home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow root");
            std::fs::write(workflows.join("role-bounds.js"), source_for_home)
                .expect("write saved workflow");
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("use in-process workflow host");
            let role_path = config.codex_home.join("oversized-role.toml");
            std::fs::write(
                &role_path,
                format!(
                    "developer_instructions = {}\n",
                    toml::Value::String("x".repeat(4_001))
                ),
            )
            .expect("write oversized role");
            config.agent_roles.insert(
                "oversized".to_string(),
                AgentRoleConfig {
                    description: Some("Must reject before spawning".to_string()),
                    config_file: Some(role_path.into_path_buf()),
                    nickname_candidates: None,
                },
            );
        });
    let test = builder.build_with_auto_env(&server).await?;
    let mut events = test.codex.subscribe_events();

    test.submit_turn("run the role bounds workflow").await?;
    let mut workflow_events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .context("timed out waiting for bounded workflow completion")??;
        let EventMsg::Workflow(workflow_event) = event.msg else {
            continue;
        };
        let terminal = matches!(workflow_event, WorkflowEvent::RunEnd(_));
        workflow_events.push(workflow_event);
        if terminal {
            break;
        }
    }

    assert!(
        !workflow_events
            .iter()
            .any(|event| matches!(event, WorkflowEvent::AgentBound(_))),
        "an invalid role must be rejected before durable child binding"
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| !request.body_contains_text(CHILD_PROMPT_MARKER)),
        "an invalid role must not reach the child model provider"
    );

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_inherited_context_rejects_before_a_child_provider_request() -> Result<()> {
    let server = responses::start_mock_server().await;
    let workflow_source = format!(
        r#"export const meta = {{ name: '{WORKFLOW_NAME}', description: 'inherited context bound' }};
const result = await agent('{INHERITED_CONTEXT_CHILD_PROMPT_MARKER}');
log('INHERITED_CONTEXT_RESULT:' + JSON.stringify(result));
text(JSON.stringify(result));
"#
    );
    let workflow_args = json!({"name": WORKFLOW_NAME}).to_string();
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-inherited-parent-open"),
                ev_function_call(WORKFLOW_CALL_ID, "workflow_run", &workflow_args),
                ev_completed("resp-inherited-parent-open"),
            ]),
            sse(vec![
                ev_response_created("resp-inherited-parent-final"),
                ev_assistant_message("msg-inherited-parent-final", "workflow admitted"),
                ev_completed("resp-inherited-parent-final"),
            ]),
        ],
    )
    .await;

    let server_uri = server.uri();
    let source_for_home = workflow_source.clone();
    let test = test_codex()
        .with_pre_build_hook(move |home| {
            std::fs::write(
                home.join("config.toml"),
                format!("openai_base_url = \"{server_uri}/v1\"\n"),
            )
            .expect("write mock provider config");
            let workflows = home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow root");
            std::fs::write(workflows.join("role-bounds.js"), source_for_home)
                .expect("write saved workflow");
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("use in-process workflow host");
            // This is one byte above the tokenizer-independent workflow-child item ceiling while
            // remaining valid parent configuration.
            config.developer_instructions = Some("x".repeat(WORKFLOW_CUSTOM_CONTEXT_MAX_BYTES + 1));
        })
        .build_with_auto_env(&server)
        .await?;
    let mut events = test.codex.subscribe_events();

    test.submit_turn("run the inherited context bounds workflow")
        .await?;
    let mut workflow_events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .context("timed out waiting for inherited-context workflow completion")??;
        let EventMsg::Workflow(workflow_event) = event.msg else {
            continue;
        };
        let terminal = matches!(workflow_event, WorkflowEvent::RunEnd(_));
        workflow_events.push(workflow_event);
        if terminal {
            break;
        }
    }

    assert!(
        !workflow_events
            .iter()
            .any(|event| matches!(event, WorkflowEvent::AgentBound(_))),
        "oversized inherited context must be rejected before durable child binding"
    );
    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| !request.body_contains_text(INHERITED_CONTEXT_CHILD_PROMPT_MARKER)),
        "oversized inherited context must not reach the child model provider"
    );

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_inherited_developer_context_is_a_standalone_bounded_item() -> Result<()> {
    let server = responses::start_mock_server().await;
    let workflow_source = format!(
        r#"export const meta = {{ name: '{WORKFLOW_NAME}', description: 'accepted context bound' }};
const result = await agent('{ACCEPTED_CONTEXT_CHILD_PROMPT_MARKER} ${OVERSIZED_SKILL_NAME}');
text(JSON.stringify(result));
"#
    );
    let workflow_args = json!({"name": WORKFLOW_NAME}).to_string();
    let _parent_open_mock = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !request_is_subagent(request) && !request_body_contains(request, WORKFLOW_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-accepted-parent-open"),
            ev_function_call(WORKFLOW_CALL_ID, "workflow_run", &workflow_args),
            ev_completed("resp-accepted-parent-open"),
        ]),
    )
    .await;
    let _parent_close_mock = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !request_is_subagent(request) && request_body_contains(request, WORKFLOW_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-accepted-parent-final"),
            ev_assistant_message("msg-accepted-parent-final", "workflow admitted"),
            ev_completed("resp-accepted-parent-final"),
        ]),
    )
    .await;
    let first_child_mock = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_subagent(request)
                && request_body_contains(request, ACCEPTED_CONTEXT_CHILD_PROMPT_MARKER)
                && !request_body_contains(request, "accepted-child-tool")
        },
        sse(vec![
            ev_response_created("resp-accepted-child-tool"),
            ev_function_call(
                "accepted-child-tool",
                "shell_command",
                &json!({
                    "command": "echo bounded",
                    "login": false,
                    "timeout_ms": 10_000,
                })
                .to_string(),
            ),
            ev_completed("resp-accepted-child-tool"),
        ]),
    )
    .await;
    let follow_up_child_mock = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_is_subagent(request) && request_body_contains(request, "accepted-child-tool")
        },
        sse(vec![
            ev_response_created("resp-accepted-child-final"),
            ev_assistant_message("msg-accepted-child-final", "bounded child"),
            ev_completed("resp-accepted-child-final"),
        ]),
    )
    .await;

    let inherited_developer = "d".repeat(WORKFLOW_CUSTOM_CONTEXT_MAX_BYTES);
    let developer_for_config = inherited_developer.clone();
    let server_uri = server.uri();
    let source_for_home = workflow_source.clone();
    let test = test_codex()
        .with_pre_build_hook(move |home| {
            std::fs::write(
                home.join("config.toml"),
                format!("openai_base_url = \"{server_uri}/v1\"\n"),
            )
            .expect("write mock provider config");
            let workflows = home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow root");
            std::fs::write(workflows.join("role-bounds.js"), source_for_home)
                .expect("write saved workflow");
            std::fs::write(
                home.join("AGENTS.md"),
                format!("GLOBAL_OPEN{}GLOBAL_CLOSE", "g".repeat(20_000)),
            )
            .expect("write oversized global instructions");
            let skill_dir = home.join("skills").join(OVERSIZED_SKILL_NAME);
            std::fs::create_dir_all(&skill_dir).expect("create oversized skill root");
            std::fs::write(
                skill_dir.join("SKILL.md"),
                format!(
                    "---\nname: {OVERSIZED_SKILL_NAME}\ndescription: context bound fixture\n---\n\nSKILL_OPEN{}SKILL_CLOSE\n",
                    "s".repeat(20_000)
                ),
            )
            .expect("write oversized skill");
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("use in-process workflow host");
            config.developer_instructions = Some(developer_for_config);
        })
        .build_with_auto_env(&server)
        .await?;
    let mut events = test.codex.subscribe_events();

    test.submit_turn("run the accepted context bounds workflow")
        .await?;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .context("timed out waiting for accepted-context workflow completion")??;
        if matches!(event.msg, EventMsg::Workflow(WorkflowEvent::RunEnd(_))) {
            break;
        }
    }

    let first_child_request = first_child_mock.single_request();
    let follow_up_child_request = follow_up_child_mock.single_request();
    let first_tool_names = bounded_request_tool_names(&first_child_request);
    let follow_up_tool_names = bounded_request_tool_names(&follow_up_child_request);
    assert_eq!(follow_up_tool_names, first_tool_names);
    assert!(
        first_tool_names
            .iter()
            .any(|name| name == "shell_command" || name == "exec_command"),
        "workflow child must retain an execution tool: {first_tool_names:?}"
    );
    assert!(
        first_tool_names.iter().any(|name| name == "apply_patch"),
        "workflow child must retain apply_patch: {first_tool_names:?}"
    );
    let child_requests = [&first_child_request, &follow_up_child_request];
    let first_developer_groups = first_child_request.message_input_text_groups("developer");
    let first_user_groups = first_child_request.message_input_text_groups("user");
    let inherited_prefix = "d".repeat(256);
    let finalized_inherited_developer = first_developer_groups
        .iter()
        .find_map(|group| match group.as_slice() {
            [text] if text.starts_with(&inherited_prefix) => Some(text),
            _ => None,
        })
        .context("expected finalized inherited developer context")?;
    assert!(finalized_inherited_developer.len() < inherited_developer.len());
    assert!(
        finalized_inherited_developer.contains("\n... [workflow context truncated] ...\n")
            || finalized_inherited_developer
                .contains("\n... [workflow finalized context truncated] ...\n")
    );
    let finalized_inherited_item = first_child_request
        .inputs_of_type("message")
        .into_iter()
        .find(|item| {
            item.get("role").and_then(Value::as_str) == Some("developer")
                && item["content"].as_array().is_some_and(|content| {
                    content.iter().any(|span| {
                        span.get("text").and_then(Value::as_str)
                            == Some(finalized_inherited_developer.as_str())
                    })
                })
        })
        .context("expected finalized inherited developer item")?;
    assert!(
        serde_json::to_vec(&finalized_inherited_item)
            .expect("finalized inherited developer item should serialize")
            .len()
            <= WORKFLOW_CUSTOM_CONTEXT_MAX_BYTES
    );
    for request in child_requests {
        for group in request
            .message_input_text_groups("developer")
            .into_iter()
            .chain(request.message_input_text_groups("user"))
        {
            assert_eq!(
                group.len(),
                1,
                "workflow context must not aggregate text parts"
            );
            assert!(
                group[0].len() <= WORKFLOW_CUSTOM_CONTEXT_MAX_BYTES
                    || codex_models_manager::model_info::is_audited_model_instruction(&group[0]),
                "workflow context item exceeded the hard byte cap without matching audited product context: {}",
                group[0].len()
            );
            assert!(!group[0].contains("<multi_agent"));
        }
    }
    assert_eq!(
        follow_up_child_request.message_input_text_groups("developer"),
        first_developer_groups
    );
    assert_eq!(
        follow_up_child_request.message_input_text_groups("user"),
        first_user_groups
    );
    let skill_fragment = first_user_groups
        .iter()
        .flat_map(|group| group.iter())
        .find(|text| text.starts_with("<skill>"))
        .context("expected bounded explicit skill injection")?;
    assert!(skill_fragment.contains("SKILL_OPEN"));
    assert!(skill_fragment.contains("SKILL_CLOSE"));
    assert!(skill_fragment.contains("[workflow context truncated]"));

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_child_aggregates_two_environment_project_docs_within_hard_cap() -> Result<()> {
    skip_if_no_network!(Ok(()));
    skip_if_no_remote_env!(Ok(()));

    let server = responses::start_mock_server().await;
    let workflow_source = format!(
        r#"export const meta = {{ name: '{WORKFLOW_NAME}', description: 'project doc bound' }};
const result = await agent('{PROJECT_DOC_CHILD_PROMPT_MARKER}');
text(JSON.stringify(result));
"#
    );
    let workflow_args = json!({"name": WORKFLOW_NAME}).to_string();
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-project-parent-open"),
                ev_function_call(WORKFLOW_CALL_ID, "workflow_run", &workflow_args),
                ev_completed("resp-project-parent-open"),
            ]),
            sse(vec![
                ev_response_created("resp-project-child"),
                ev_assistant_message("msg-project-child", "bounded child"),
                ev_completed("resp-project-child"),
            ]),
            sse(vec![
                ev_response_created("resp-project-parent-final"),
                ev_assistant_message("msg-project-parent-final", "workflow admitted"),
                ev_completed("resp-project-parent-final"),
            ]),
        ],
    )
    .await;

    let local_root = TempDir::new()?;
    let local_doc = "L".repeat(WORKFLOW_PROJECT_DOC_MAX_BYTES * 2);
    std::fs::write(local_root.path().join("AGENTS.md"), &local_doc)?;
    let server_uri = server.uri();
    let source_for_home = workflow_source.clone();
    let mut builder = test_codex()
        .with_pre_build_hook(move |home| {
            std::fs::write(
                home.join("config.toml"),
                format!("openai_base_url = \"{server_uri}/v1\"\n"),
            )
            .expect("write mock provider config");
            let workflows = home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow root");
            std::fs::write(workflows.join("role-bounds.js"), source_for_home)
                .expect("write saved workflow");
        })
        .with_workspace_setup(|cwd, fs| async move {
            fs.write_file(
                &PathUri::from_host_native_path(cwd.join("AGENTS.md"))?,
                vec![b'R'; WORKFLOW_PROJECT_DOC_MAX_BYTES / 4],
                /*sandbox*/ None,
            )
            .await?;
            Ok(())
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("use in-process workflow host");
            // Prove the workflow path clamps an inherited setting rather than relying on the root
            // session already having the child-specific limit.
            config.project_doc_max_bytes = WORKFLOW_PROJECT_DOC_MAX_BYTES * 8;
        });
    let test = builder.build_with_remote_and_local_env(&server).await?;
    let mut events = test.codex.subscribe_events();
    test.submit_turn_with_environments(
        "run the bounded project-doc workflow",
        Some(vec![
            TurnEnvironmentSelection {
                environment_id: REMOTE_ENVIRONMENT_ID.to_string(),
                cwd: PathUri::from_abs_path(&test.config.cwd),
            },
            TurnEnvironmentSelection {
                environment_id: LOCAL_ENVIRONMENT_ID.to_string(),
                cwd: PathUri::from_host_native_path(local_root.path())?,
            },
        ]),
    )
    .await?;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .context("timed out waiting for bounded project-doc workflow completion")??;
        if matches!(event.msg, EventMsg::Workflow(WorkflowEvent::RunEnd(_))) {
            break;
        }
    }

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 3);
    let child_request = requests
        .iter()
        .find(|request| request.body_contains_text(PROJECT_DOC_CHILD_PROMPT_MARKER))
        .context("expected workflow child provider request")?;
    let project_fragment = child_request
        .message_input_texts("user")
        .into_iter()
        .find(|text| text.starts_with("# AGENTS.md instructions"))
        .context("expected project instructions in workflow child request")?;
    assert!(project_fragment.contains(&format!("for `{REMOTE_ENVIRONMENT_ID}`")));
    assert!(project_fragment.contains(&format!("for `{LOCAL_ENVIRONMENT_ID}`")));
    assert!(project_fragment.len() <= WORKFLOW_PROJECT_DOC_MAX_BYTES);
    assert!(codex_utils_output_truncation::approx_token_count(&project_fragment) < 10_000);
    assert!(!project_fragment.contains(&local_doc));

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

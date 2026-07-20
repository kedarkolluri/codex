use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::CollabAgentStatus;
use codex_app_server_protocol::JSONRPCError;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::JSONRPCNotification;
use codex_app_server_protocol::JSONRPCResponse;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadDeleteParams;
use codex_app_server_protocol::ThreadDeleteResponse;
use codex_app_server_protocol::ThreadDeletedNotification;
use codex_app_server_protocol::ThreadInjectItemsParams;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TokenUsageBreakdown;
use codex_app_server_protocol::WorkflowAgentBoundNotification;
use codex_app_server_protocol::WorkflowAgentCompletedNotification;
use codex_app_server_protocol::WorkflowAgentStartedNotification;
use codex_app_server_protocol::WorkflowAgentUpdatedNotification;
use codex_app_server_protocol::WorkflowCompletedNotification;
use codex_app_server_protocol::WorkflowGroupCompletedNotification;
use codex_app_server_protocol::WorkflowGroupKind;
use codex_app_server_protocol::WorkflowGroupStartedNotification;
use codex_app_server_protocol::WorkflowPhaseChangedNotification;
use codex_app_server_protocol::WorkflowPhaseStatus;
use codex_app_server_protocol::WorkflowRunTerminalReason;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_core::RolloutRecorder;
use codex_features::Feature;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use core_test_support::TestTargetOs;
use core_test_support::responses;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use serde::de::DeserializeOwned;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const CHILD_PROMPT: &str = "APP_SERVER_WORKFLOW_LIFECYCLE_CHILD";
const CHILD_LABEL: &str = "app-lifecycle-child";
const TOOL_CALL_ID: &str = "app-workflow-lifecycle-tool";
const WORKFLOW_BUDGET_TOTAL: i64 = 4_096;
const FIRST_RESPONSE_OUTPUT_TOKENS: i64 = 13;
const SECOND_RESPONSE_OUTPUT_TOKENS: i64 = 17;
const TOTAL_OUTPUT_TOKENS: i64 = FIRST_RESPONSE_OUTPUT_TOKENS + SECOND_RESPONSE_OUTPUT_TOKENS;

fn write_config(codex_home: &Path, server_uri: &str, workflow_enabled: bool) -> Result<()> {
    let features = BTreeMap::from([(Feature::Workflow, workflow_enabled)]);
    write_mock_responses_config_toml(
        codex_home,
        server_uri,
        &features,
        /*auto_compact_limit*/ 100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;
    let config_path = codex_home.join("config.toml");
    let config = std::fs::read_to_string(&config_path)?;
    std::fs::write(
        config_path,
        config.replace(
            "sandbox_mode = \"read-only\"",
            "sandbox_mode = \"danger-full-access\"",
        ),
    )?;
    Ok(())
}

fn write_workflow(codex_home: &Path, name: &str, phases: &[&str], body: &str) -> Result<()> {
    let workflows = codex_home.join("workflows");
    std::fs::create_dir_all(&workflows)?;
    let phases = serde_json::to_string(phases)?;
    std::fs::write(
        workflows.join(format!("{name}.workflow.js")),
        format!(
            "export const meta = {{ name: '{name}', description: 'integration test', phases: {phases} }};\n{body}\n"
        ),
    )?;
    Ok(())
}

async fn read_response<T: DeserializeOwned>(mcp: &mut TestAppServer, request_id: i64) -> Result<T> {
    let response: JSONRPCResponse = timeout(
        DEFAULT_TIMEOUT,
        mcp.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    to_response::<T>(response)
}

async fn start_thread(mcp: &mut TestAppServer) -> Result<String> {
    let cwd = mcp.auto_env()?.cwd().clone().into_path_buf();
    let request_id = mcp
        .send_thread_start_request_with_auto_env(ThreadStartParams {
            cwd: Some(cwd.to_string_lossy().into_owned()),
            ..Default::default()
        })
        .await?;
    Ok(read_response::<ThreadStartResponse>(mcp, request_id)
        .await?
        .thread
        .id)
}

async fn start_workflow(
    mcp: &mut TestAppServer,
    thread_id: &str,
    name: &str,
    args: Option<Value>,
) -> Result<i64> {
    mcp.send_workflow_start_request(WorkflowStartParams {
        thread_id: thread_id.to_string(),
        name: name.to_string(),
        args,
    })
    .await
}

fn completed_with_output_tokens(id: &str, output_tokens: i64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": 0,
                "input_tokens_details": null,
                "output_tokens": output_tokens,
                "output_tokens_details": null,
                "total_tokens": output_tokens
            }
        }
    })
}

fn lifecycle_workflow_body() -> &'static str {
    r#"phase('verify');
const [result] = await parallel([
  () => agent('APP_SERVER_WORKFLOW_LIFECYCLE_CHILD', {
    label: 'app-lifecycle-child',
    phase: 'verify',
  }),
]);
text(result);"#
}

fn shell_command_args(command: &str) -> Result<String> {
    Ok(serde_json::to_string(&json!({
        "command": command,
        "login": false,
        "timeout_ms": 10_000,
    }))?)
}

fn indefinitely_blocking_command() -> &'static str {
    match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "tail -f /dev/null",
        TestTargetOs::Windows => "[System.Threading.ManualResetEvent]::new($false).WaitOne()",
    }
}

fn notification_params<T: DeserializeOwned>(notification: &JSONRPCNotification) -> Result<T> {
    let params = notification
        .params
        .clone()
        .with_context(|| format!("{} notification has no params", notification.method))?;
    serde_json::from_value(params)
        .with_context(|| format!("deserialize {} notification", notification.method))
}

fn notifications_for_method<T: DeserializeOwned>(
    notifications: &[JSONRPCNotification],
    method: &str,
) -> Result<Vec<T>> {
    notifications
        .iter()
        .filter(|notification| notification.method == method)
        .map(notification_params)
        .collect()
}

fn notification_run_id(notification: &JSONRPCNotification) -> Option<&str> {
    notification.params.as_ref()?.get("runId")?.as_str()
}

async fn read_run_notifications_until<F>(
    mcp: &mut TestAppServer,
    run_id: &str,
    mut finished: F,
) -> Result<Vec<JSONRPCNotification>>
where
    F: FnMut(&JSONRPCNotification) -> bool,
{
    timeout(DEFAULT_TIMEOUT, async {
        let mut notifications = Vec::new();
        loop {
            let message = mcp.read_next_message().await?;
            let JSONRPCMessage::Notification(notification) = message else {
                continue;
            };
            if notification_run_id(&notification) != Some(run_id) {
                continue;
            }
            let is_finished = finished(&notification);
            notifications.push(notification);
            if is_finished {
                return Ok::<_, anyhow::Error>(notifications);
            }
        }
    })
    .await?
}

fn read_json(path: &Path) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn read_json_lines(path: &Path) -> Result<Vec<Value>> {
    std::fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_start_streams_real_child_lifecycle_with_exact_terminal_counters() -> Result<()> {
    let server = responses::start_mock_server().await;
    let tool_args = shell_command_args("echo APP_WORKFLOW_TOOL_OK")?;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("app-workflow-child-tool"),
                responses::ev_function_call(TOOL_CALL_ID, "shell_command", &tool_args),
                completed_with_output_tokens(
                    "app-workflow-child-tool",
                    FIRST_RESPONSE_OUTPUT_TOKENS,
                ),
            ]),
            responses::sse(vec![
                responses::ev_response_created("app-workflow-child-complete"),
                responses::ev_assistant_message(
                    "app-workflow-child-message",
                    "lifecycle-child-done",
                ),
                completed_with_output_tokens(
                    "app-workflow-child-complete",
                    SECOND_RESPONSE_OUTPUT_TOKENS,
                ),
            ]),
        ],
    )
    .await;

    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    write_workflow(
        codex_home.path(),
        "app-lifecycle",
        &["verify"],
        lifecycle_workflow_body(),
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;

    let request_id = start_workflow(
        &mut mcp,
        &thread_id,
        "app-lifecycle",
        Some(json!({"budget": {"total": WORKFLOW_BUDGET_TOTAL}})),
    )
    .await?;
    let WorkflowStartResponse { run_id } =
        read_response::<WorkflowStartResponse>(&mut mcp, request_id).await?;
    uuid::Uuid::parse_str(&run_id)?;

    let notifications = read_run_notifications_until(&mut mcp, &run_id, |notification| {
        notification.method == "workflow/completed"
    })
    .await?;
    let major_methods = notifications
        .iter()
        .filter(|notification| notification.method != "workflow/agent/updated")
        .map(|notification| notification.method.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        major_methods,
        vec![
            "workflow/started",
            "workflow/phase/changed",
            "workflow/group/started",
            "workflow/agent/started",
            "workflow/agent/bound",
            "workflow/agent/completed",
            "workflow/group/completed",
            "workflow/phase/changed",
            "workflow/completed",
        ]
    );

    let started = notifications_for_method::<WorkflowStartedNotification>(
        &notifications,
        "workflow/started",
    )?
    .pop()
    .context("workflow/started")?;
    assert!(started.started_at > 0);
    assert!(!started.args_digest.is_empty());
    assert_eq!(
        started,
        WorkflowStartedNotification {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            resumed_from_run_id: None,
            name: "app-lifecycle".to_string(),
            phases: vec!["verify".to_string()],
            args_digest: started.args_digest.clone(),
            started_at: started.started_at,
        }
    );

    let phases = notifications_for_method::<WorkflowPhaseChangedNotification>(
        &notifications,
        "workflow/phase/changed",
    )?;
    assert_eq!(phases.len(), 2);
    assert_eq!(
        phases
            .iter()
            .map(|phase| (
                phase.thread_id.as_str(),
                phase.run_id.as_str(),
                phase.phase_index,
                phase.title.as_str(),
                phase.status,
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                thread_id.as_str(),
                run_id.as_str(),
                0,
                "verify",
                WorkflowPhaseStatus::Active,
            ),
            (
                thread_id.as_str(),
                run_id.as_str(),
                0,
                "verify",
                WorkflowPhaseStatus::Completed,
            ),
        ]
    );
    assert!(phases.iter().all(|phase| phase.changed_at > 0));

    let group_started = notifications_for_method::<WorkflowGroupStartedNotification>(
        &notifications,
        "workflow/group/started",
    )?
    .pop()
    .context("workflow/group/started")?;
    assert_eq!(
        (
            group_started.thread_id.as_str(),
            group_started.run_id.as_str(),
            group_started.group_id,
            group_started.parent_node_id,
            group_started.kind,
            group_started.item_count,
        ),
        (
            thread_id.as_str(),
            run_id.as_str(),
            0,
            None,
            WorkflowGroupKind::Parallel,
            1,
        )
    );
    assert!(group_started.started_at > 0);

    let agent_started = notifications_for_method::<WorkflowAgentStartedNotification>(
        &notifications,
        "workflow/agent/started",
    )?
    .pop()
    .context("workflow/agent/started")?;
    assert_eq!(
        (
            agent_started.thread_id.as_str(),
            agent_started.run_id.as_str(),
            agent_started.node_id,
            agent_started.attempt,
            agent_started.last_attempt_reason,
            agent_started.parent_node_id,
            agent_started.label.as_str(),
            agent_started.phase.as_deref(),
            agent_started.model.as_str(),
        ),
        (
            thread_id.as_str(),
            run_id.as_str(),
            1,
            0,
            None,
            Some(0),
            CHILD_LABEL,
            Some("verify"),
            "mock-model",
        )
    );
    assert!(agent_started.started_at > 0);

    let bound = notifications_for_method::<WorkflowAgentBoundNotification>(
        &notifications,
        "workflow/agent/bound",
    )?
    .pop()
    .context("workflow/agent/bound")?;
    uuid::Uuid::parse_str(&bound.child_thread_id)?;
    assert_ne!(bound.child_thread_id, thread_id);
    assert_eq!(
        (
            bound.thread_id.as_str(),
            bound.run_id.as_str(),
            bound.node_id,
            bound.attempt,
        ),
        (thread_id.as_str(), run_id.as_str(), 1, 0)
    );
    assert!(bound.bound_at > 0);

    let read_child_id = mcp
        .send_thread_read_request(ThreadReadParams {
            thread_id: bound.child_thread_id.clone(),
            include_turns: false,
        })
        .await?;
    let ThreadReadResponse {
        thread: child_thread,
    } = read_response::<ThreadReadResponse>(&mut mcp, read_child_id).await?;
    let child_rollout_path = child_thread
        .path
        .as_ref()
        .context("workflow child rollout path")?;
    let history_before_rejected_injections =
        RolloutRecorder::get_rollout_history(child_rollout_path).await?;
    let rejected_items = [
        ResponseItem::AgentMessage {
            id: None,
            author: "external-parent".to_string(),
            recipient: bound.child_thread_id.clone(),
            content: vec![AgentMessageInputContent::InputText {
                text: "must not enter workflow child history".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::FunctionCall {
            id: None,
            name: "untrusted_call".to_string(),
            namespace: None,
            arguments: "{}".to_string(),
            call_id: "untrusted-call".to_string(),
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    for item in rejected_items {
        let inject_id = mcp
            .send_thread_inject_items_request(ThreadInjectItemsParams {
                thread_id: bound.child_thread_id.clone(),
                items: vec![serde_json::to_value(item)?],
            })
            .await?;
        let error: JSONRPCError = timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_error_message(RequestId::Integer(inject_id)),
        )
        .await??;
        assert_eq!(
            error.error.message,
            "workflow-managed threads accept only text message injection"
        );
    }
    let history_after_rejected_injections =
        RolloutRecorder::get_rollout_history(child_rollout_path).await?;
    assert_eq!(
        serde_json::to_value(history_after_rejected_injections)?,
        serde_json::to_value(history_before_rejected_injections)?,
        "rejected app-server injection must not mutate the child rollout"
    );

    let updates = notifications_for_method::<WorkflowAgentUpdatedNotification>(
        &notifications,
        "workflow/agent/updated",
    )?;
    assert!(!updates.is_empty());
    for pair in updates.windows(2) {
        assert!(pair[0].token_usage.total_tokens <= pair[1].token_usage.total_tokens);
        assert!(pair[0].tool_call_count <= pair[1].tool_call_count);
        assert!(pair[0].duration_ms <= pair[1].duration_ms);
    }
    let expected_usage = TokenUsageBreakdown {
        total_tokens: TOTAL_OUTPUT_TOKENS,
        input_tokens: 0,
        cached_input_tokens: 0,
        output_tokens: TOTAL_OUTPUT_TOKENS,
        reasoning_output_tokens: 0,
    };
    let terminal_update = updates.last().context("terminal workflow/agent/updated")?;
    assert_eq!(
        terminal_update,
        &WorkflowAgentUpdatedNotification {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            token_usage: expected_usage.clone(),
            tool_call_count: 1,
            duration_ms: terminal_update.duration_ms,
            updated_at: terminal_update.updated_at,
        }
    );
    assert!(terminal_update.updated_at > 0);

    let agent_completed = notifications_for_method::<WorkflowAgentCompletedNotification>(
        &notifications,
        "workflow/agent/completed",
    )?
    .pop()
    .context("workflow/agent/completed")?;
    assert_eq!(
        agent_completed,
        WorkflowAgentCompletedNotification {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            node_id: 1,
            attempt: 0,
            last_attempt_reason: None,
            status: CollabAgentStatus::Completed,
            message: None,
            token_usage: expected_usage,
            tool_call_count: 1,
            duration_ms: agent_completed.duration_ms,
            returned_null: false,
            completed_at: agent_completed.completed_at,
        }
    );
    assert!(agent_completed.completed_at > 0);

    let group_completed = notifications_for_method::<WorkflowGroupCompletedNotification>(
        &notifications,
        "workflow/group/completed",
    )?
    .pop()
    .context("workflow/group/completed")?;
    assert_eq!(
        (
            group_completed.thread_id.as_str(),
            group_completed.run_id.as_str(),
            group_completed.group_id,
            group_completed.kind,
            group_completed.item_count,
        ),
        (
            thread_id.as_str(),
            run_id.as_str(),
            0,
            WorkflowGroupKind::Parallel,
            1,
        )
    );
    assert!(group_completed.completed_at > 0);

    let completed = notifications_for_method::<WorkflowCompletedNotification>(
        &notifications,
        "workflow/completed",
    )?
    .pop()
    .context("workflow/completed")?;
    assert_eq!(
        completed,
        WorkflowCompletedNotification {
            thread_id: thread_id.clone(),
            run_id: run_id.clone(),
            status: CollabAgentStatus::Completed,
            message: None,
            terminal_reason: Some(WorkflowRunTerminalReason::Completed),
            spent: TOTAL_OUTPUT_TOKENS,
            total: Some(WORKFLOW_BUDGET_TOTAL),
            completed_at: completed.completed_at,
        }
    );
    assert!(completed.completed_at > 0);

    let requests = response_mock.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body_contains_text(CHILD_PROMPT));
    assert!(
        requests[1]
            .function_call_output_text(TOOL_CALL_ID)
            .is_some_and(|output| output.contains("APP_WORKFLOW_TOOL_OK"))
    );

    let run_dir = codex_home.path().join("workflows/runs").join(&run_id);
    let meta = read_json(&run_dir.join("meta.json"))?;
    assert_eq!(meta["status"], "completed");
    let progress = read_json(&run_dir.join("progress.json"))?;
    assert_eq!(
        (
            progress["state"].clone(),
            progress["status"].clone(),
            progress["budget"]["spent"].clone(),
            progress["budget"]["total"].clone(),
        ),
        (
            json!("terminal"),
            json!({"completed": null}),
            json!(TOTAL_OUTPUT_TOKENS),
            json!(WORKFLOW_BUDGET_TOTAL),
        )
    );
    let journal = read_json_lines(&run_dir.join("journal.jsonl"))?;
    let agent_call = journal
        .iter()
        .find(|line| line["type"] == "agent_call")
        .context("terminal agent_call journal line")?;
    assert_eq!(
        (
            agent_call["status"].clone(),
            agent_call["return"].clone(),
            agent_call["child_thread_id"].clone(),
            agent_call["tokens_spent"].clone(),
        ),
        (
            json!("completed"),
            json!("lifecycle-child-done"),
            json!(bound.child_thread_id),
            json!(TOTAL_OUTPUT_TOKENS),
        )
    );
    Ok(())
}

/// `workflow/stop` targets one session-owned run; `thread/delete` remains the full-root shutdown
/// surface and must stop every run plus spawned descendants before replying. This test drives a
/// child into an indefinitely blocked tool call, invokes that broader shutdown path, and verifies
/// both child removal and the workflow's durable interrupted terminal record without a polling
/// sleep.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn thread_delete_cleans_up_in_flight_workflow_child_and_terminalizes_run() -> Result<()> {
    const CANCEL_OUTPUT_TOKENS: i64 = 7;

    let server = responses::start_mock_server().await;
    let tool_args = shell_command_args(indefinitely_blocking_command())?;
    let response_mock = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("app-workflow-cancel-tool"),
            responses::ev_function_call(TOOL_CALL_ID, "shell_command", &tool_args),
            completed_with_output_tokens("app-workflow-cancel-tool", CANCEL_OUTPUT_TOKENS),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    write_config(
        codex_home.path(),
        &server.uri(),
        /*workflow_enabled*/ true,
    )?;
    write_workflow(
        codex_home.path(),
        "app-cancel",
        &["verify"],
        lifecycle_workflow_body(),
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let thread_id = start_thread(&mut mcp).await?;
    let request_id = start_workflow(
        &mut mcp,
        &thread_id,
        "app-cancel",
        Some(json!({"budget": {"total": WORKFLOW_BUDGET_TOTAL}})),
    )
    .await?;
    let WorkflowStartResponse { run_id } =
        read_response::<WorkflowStartResponse>(&mut mcp, request_id).await?;

    let in_flight = read_run_notifications_until(&mut mcp, &run_id, |notification| {
        if notification.method != "workflow/agent/updated" {
            return false;
        }
        notification_params::<WorkflowAgentUpdatedNotification>(notification).is_ok_and(|updated| {
            updated.token_usage.total_tokens == CANCEL_OUTPUT_TOKENS && updated.tool_call_count == 1
        })
    })
    .await?;
    let bound = notifications_for_method::<WorkflowAgentBoundNotification>(
        &in_flight,
        "workflow/agent/bound",
    )?
    .pop()
    .context("in-flight workflow child binding")?;
    uuid::Uuid::parse_str(&bound.child_thread_id)?;
    assert!(
        response_mock
            .single_request()
            .body_contains_text(CHILD_PROMPT)
    );

    let delete_request_id = mcp
        .send_thread_delete_request(ThreadDeleteParams {
            thread_id: thread_id.clone(),
        })
        .await?;
    let _: ThreadDeleteResponse = read_response(&mut mcp, delete_request_id).await?;

    let mut deleted_thread_ids = Vec::new();
    for _ in 0..2 {
        let notification = timeout(
            DEFAULT_TIMEOUT,
            mcp.read_stream_until_notification_message("thread/deleted"),
        )
        .await??;
        let deleted: ThreadDeletedNotification = notification_params(&notification)?;
        deleted_thread_ids.push(deleted.thread_id);
    }
    assert_eq!(
        deleted_thread_ids,
        vec![bound.child_thread_id.clone(), thread_id.clone()]
    );

    let list_request_id = mcp
        .send_thread_loaded_list_request(ThreadLoadedListParams::default())
        .await?;
    let loaded: ThreadLoadedListResponse = read_response(&mut mcp, list_request_id).await?;
    assert_eq!(loaded.data, Vec::<String>::new());

    let run_dir = codex_home.path().join("workflows/runs").join(&run_id);
    let meta = read_json(&run_dir.join("meta.json"))?;
    assert_eq!(meta["status"], "failed");
    let progress = read_json(&run_dir.join("progress.json"))?;
    assert_eq!(
        (
            progress["state"].clone(),
            progress["status"].clone(),
            progress["budget"]["total"].clone(),
        ),
        (
            json!("terminal"),
            json!("interrupted"),
            json!(WORKFLOW_BUDGET_TOTAL),
        )
    );
    let agent = &progress["topology"]["1"];
    assert_eq!(
        (
            agent["node_type"].clone(),
            agent["child_thread_id"].clone(),
            agent["state"].clone(),
            agent["status"].clone(),
            agent["token_usage"]["total_tokens"].clone(),
            agent["tool_call_count"].clone(),
            agent["returned_null"].clone(),
        ),
        (
            json!("agent"),
            json!(bound.child_thread_id.clone()),
            json!("completed"),
            json!("interrupted"),
            json!(CANCEL_OUTPUT_TOKENS),
            json!(1),
            json!(true),
        )
    );
    let journal = read_json_lines(&run_dir.join("journal.jsonl"))?;
    let agent_call = journal
        .iter()
        .find(|line| line["type"] == "agent_call")
        .context("cancelled agent_call journal line")?;
    assert_eq!(
        (
            agent_call["return"].clone(),
            agent_call["child_thread_id"].clone(),
        ),
        (Value::Null, json!(bound.child_thread_id))
    );
    Ok(())
}

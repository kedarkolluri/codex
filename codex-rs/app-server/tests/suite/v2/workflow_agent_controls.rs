use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::WorkflowAgentControlAction;
use codex_app_server_protocol::WorkflowAgentControlParams;
use codex_app_server_protocol::WorkflowAgentControlResponse;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

use super::workflow_controls::AGENT_UNAVAILABLE;
use super::workflow_controls::DEFAULT_TIMEOUT;
use super::workflow_controls::assert_bounded_error;
use super::workflow_controls::blocking_command;
use super::workflow_controls::completed_with_output_tokens;
use super::workflow_controls::read_error;
use super::workflow_controls::read_response;
use super::workflow_controls::start_thread;
use super::workflow_controls::start_workflow;
use super::workflow_controls::wait_for_blocked_agent;
use super::workflow_controls::wait_for_completed;
use super::workflow_controls::write_config;
use super::workflow_controls::write_workflow;

#[derive(Debug, PartialEq, Eq)]
enum ControlOutcome {
    Response(WorkflowAgentControlResponse),
    Error(String),
}

async fn read_control_outcomes(
    mcp: &mut TestAppServer,
    request_ids: [i64; 2],
) -> Result<Vec<ControlOutcome>> {
    let mut outcomes = Vec::new();
    timeout(DEFAULT_TIMEOUT, async {
        while outcomes.len() < request_ids.len() {
            match mcp.read_next_message().await? {
                JSONRPCMessage::Response(response)
                    if request_id_is_selected(&response.id, &request_ids) =>
                {
                    outcomes.push(ControlOutcome::Response(to_response(response)?));
                }
                JSONRPCMessage::Error(error) if request_id_is_selected(&error.id, &request_ids) => {
                    outcomes.push(ControlOutcome::Error(error.error.message));
                }
                _ => {}
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await??;
    Ok(outcomes)
}

fn request_id_is_selected(request_id: &RequestId, selected: &[i64; 2]) -> bool {
    match request_id {
        RequestId::Integer(request_id) => selected.contains(request_id),
        RequestId::String(_) => false,
    }
}

async fn mount_control_response_sequence(server: &wiremock::MockServer, bodies: Vec<String>) {
    struct SequenceResponder {
        num_calls: AtomicUsize,
        bodies: Vec<String>,
    }

    impl Respond for SequenceResponder {
        fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
            let call_num = self.num_calls.fetch_add(1, Ordering::SeqCst);
            let missing_response_message = format!("no response for control call {call_num}");
            responses::sse_response(
                self.bodies
                    .get(call_num)
                    .expect(&missing_response_message)
                    .clone(),
            )
        }
    }

    let num_calls = bodies.len() as u64;
    let minimum_calls = num_calls - 1;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(SequenceResponder {
            num_calls: AtomicUsize::new(0),
            bodies,
        })
        .up_to_n_times(num_calls)
        .expect(minimum_calls..=num_calls)
        .mount(server)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn agent_control_maps_exact_attempts_duplicates_retries_and_conflicts() -> Result<()> {
    let server = responses::start_mock_server().await;
    let tool_args = serde_json::to_string(&json!({
        "command": blocking_command(),
        "login": false,
        "timeout_ms": 10_000,
    }))?;
    let mut sequences = (0..4)
        .map(|index| {
            responses::sse(vec![
                responses::ev_response_created(&format!("control-agent-{index}")),
                responses::ev_function_call(
                    &format!("control-agent-shell-{index}"),
                    "shell_command",
                    &tool_args,
                ),
                completed_with_output_tokens(&format!("control-agent-{index}"), 5),
            ])
        })
        .collect::<Vec<_>>();
    sequences.push(responses::sse(vec![
        responses::ev_response_created("control-conflict-retry"),
        responses::ev_assistant_message("control-conflict-message", "retry completed"),
        completed_with_output_tokens("control-conflict-retry", 3),
    ]));
    mount_control_response_sequence(&server, sequences).await;
    let codex_home = TempDir::new()?;
    write_config(codex_home.path(), &server.uri())?;
    write_workflow(
        codex_home.path(),
        "agent-control",
        "await agent('CONTROL_AGENT', { label: 'controlled-child', phase: 'control' });",
    )?;
    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, mcp.initialize()).await??;
    let owner_thread_id = start_thread(&mut mcp).await?;
    let other_thread_id = start_thread(&mut mcp).await?;

    let duplicate_run = start_workflow(&mut mcp, &owner_thread_id, "agent-control", None).await?;
    let (node_id, attempt) = wait_for_blocked_agent(
        &mut mcp,
        &duplicate_run,
        /*expected_attempt*/ 0,
        /*expected_tool_call_count*/ 1,
    )
    .await?;
    let wrong_thread_request = mcp
        .send_workflow_agent_control_request(WorkflowAgentControlParams {
            thread_id: other_thread_id.clone(),
            run_id: duplicate_run.clone(),
            node_id,
            attempt,
            action: WorkflowAgentControlAction::Skip,
        })
        .await?;
    assert_bounded_error(
        read_error(&mut mcp, wrong_thread_request).await?,
        AGENT_UNAVAILABLE,
    );
    let duplicate_requests = [
        mcp.send_workflow_agent_control_request(WorkflowAgentControlParams {
            thread_id: owner_thread_id.clone(),
            run_id: duplicate_run.clone(),
            node_id,
            attempt,
            action: WorkflowAgentControlAction::Skip,
        })
        .await?,
        mcp.send_workflow_agent_control_request(WorkflowAgentControlParams {
            thread_id: owner_thread_id.clone(),
            run_id: duplicate_run.clone(),
            node_id,
            attempt,
            action: WorkflowAgentControlAction::Skip,
        })
        .await?,
    ];
    assert_eq!(
        read_control_outcomes(&mut mcp, duplicate_requests).await?,
        vec![
            ControlOutcome::Response(WorkflowAgentControlResponse::Skipped),
            ControlOutcome::Response(WorkflowAgentControlResponse::Skipped),
        ]
    );
    wait_for_completed(&mut mcp, &duplicate_run).await?;

    let uppercase_duplicate_run = duplicate_run.to_uppercase();
    for (thread_id, run_id, selected_attempt) in [
        (owner_thread_id.as_str(), duplicate_run.as_str(), attempt),
        (other_thread_id.as_str(), duplicate_run.as_str(), attempt),
        (
            owner_thread_id.as_str(),
            duplicate_run.as_str(),
            attempt + 1,
        ),
        (
            owner_thread_id.as_str(),
            uppercase_duplicate_run.as_str(),
            attempt,
        ),
    ] {
        let request_id = mcp
            .send_workflow_agent_control_request(WorkflowAgentControlParams {
                thread_id: thread_id.to_string(),
                run_id: run_id.to_string(),
                node_id,
                attempt: selected_attempt,
                action: WorkflowAgentControlAction::Skip,
            })
            .await?;
        assert_bounded_error(read_error(&mut mcp, request_id).await?, AGENT_UNAVAILABLE);
    }

    let retry_run = start_workflow(&mut mcp, &owner_thread_id, "agent-control", None).await?;
    let (retry_node_id, retry_attempt) = wait_for_blocked_agent(
        &mut mcp, &retry_run, /*expected_attempt*/ 0, /*expected_tool_call_count*/ 1,
    )
    .await?;
    let retry_request = mcp
        .send_workflow_agent_control_request(WorkflowAgentControlParams {
            thread_id: owner_thread_id.clone(),
            run_id: retry_run.clone(),
            node_id: retry_node_id,
            attempt: retry_attempt,
            action: WorkflowAgentControlAction::Retry,
        })
        .await?;
    assert_eq!(
        read_response::<WorkflowAgentControlResponse>(&mut mcp, retry_request).await?,
        WorkflowAgentControlResponse::RetryScheduled { attempt: 1 }
    );
    let (retried_node_id, retried_attempt) = wait_for_blocked_agent(
        &mut mcp, &retry_run, /*expected_attempt*/ 1, /*expected_tool_call_count*/ 2,
    )
    .await?;
    assert_eq!((retried_node_id, retried_attempt), (retry_node_id, 1));
    let cleanup_request = mcp
        .send_workflow_agent_control_request(WorkflowAgentControlParams {
            thread_id: owner_thread_id.clone(),
            run_id: retry_run.clone(),
            node_id: retried_node_id,
            attempt: retried_attempt,
            action: WorkflowAgentControlAction::Skip,
        })
        .await?;
    assert_eq!(
        read_response::<WorkflowAgentControlResponse>(&mut mcp, cleanup_request).await?,
        WorkflowAgentControlResponse::Skipped
    );
    wait_for_completed(&mut mcp, &retry_run).await?;

    let conflict_run = start_workflow(&mut mcp, &owner_thread_id, "agent-control", None).await?;
    let (conflict_node_id, conflict_attempt) = wait_for_blocked_agent(
        &mut mcp,
        &conflict_run,
        /*expected_attempt*/ 0,
        /*expected_tool_call_count*/ 1,
    )
    .await?;
    let conflict_requests = [
        mcp.send_workflow_agent_control_request(WorkflowAgentControlParams {
            thread_id: owner_thread_id.clone(),
            run_id: conflict_run.clone(),
            node_id: conflict_node_id,
            attempt: conflict_attempt,
            action: WorkflowAgentControlAction::Skip,
        })
        .await?,
        mcp.send_workflow_agent_control_request(WorkflowAgentControlParams {
            thread_id: owner_thread_id,
            run_id: conflict_run.clone(),
            node_id: conflict_node_id,
            attempt: conflict_attempt,
            action: WorkflowAgentControlAction::Retry,
        })
        .await?,
    ];
    let outcomes = read_control_outcomes(&mut mcp, conflict_requests).await?;
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| matches!(outcome, ControlOutcome::Error(message) if message == AGENT_UNAVAILABLE))
            .count(),
        1
    );
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| {
                matches!(
                    outcome,
                    ControlOutcome::Response(WorkflowAgentControlResponse::Skipped)
                        | ControlOutcome::Response(WorkflowAgentControlResponse::RetryScheduled {
                            attempt: 1
                        })
                )
            })
            .count(),
        1
    );
    wait_for_completed(&mut mcp, &conflict_run).await?;
    Ok(())
}

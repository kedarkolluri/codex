#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Real-stack coverage for exact selected-attempt Skip/Retry controls.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_core::WorkflowAgentControlAction;
use codex_core::WorkflowAgentControlDisposition;
use codex_features::Feature;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::WorkflowAgentAttemptReason;
use codex_protocol::protocol::WorkflowEvent;
use codex_workflow_journal::AgentControlReason;
use codex_workflow_journal::AgentStatus as JournalAgentStatus;
use codex_workflow_journal::ReplayJournal;
use codex_workflow_journal::RunAgentJournal;
use codex_workflow_journal::storage::WorkflowRunPaths;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const WORKFLOW_NAME: &str = "selected-agent-controls";
const RETRY_MARKER: &str = "RETRY_CONTROL_TARGET";
const SKIP_MARKER: &str = "SKIP_CONTROL_TARGET";
const WORKFLOW_SOURCE: &str = r#"export const meta = {
  name: 'selected-agent-controls',
  description: 'exact live agent control integration',
  phases: ['control'],
};
phase('control');
const retried = await agent('RETRY_CONTROL_TARGET', { label: 'retry-target' });
const skipped = await agent('SKIP_CONTROL_TARGET', { label: 'skip-target' });
log('CONTROL_RESULT:' + JSON.stringify({ retried, skipped }));
text('done');
"#;

#[derive(Clone, Default)]
struct ControlRouter {
    requests: Arc<Mutex<usize>>,
}

impl Respond for ControlRouter {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        {
            let mut count = self.requests.lock().unwrap();
            *count += 1;
        }
        let body: Value = serde_json::from_slice(&request.body).expect("request JSON");
        let body = body.to_string();
        match (body.contains(RETRY_MARKER), body.contains(SKIP_MARKER)) {
            (true, false) => {
                child_response("retry-attempt", "retry-ok").set_delay(Duration::from_secs(2))
            }
            (false, true) => {
                child_response("skip-attempt", "too-late").set_delay(Duration::from_secs(2))
            }
            _ => ResponseTemplate::new(500),
        }
    }
}

fn child_response(id: &str, message: &str) -> ResponseTemplate {
    sse_response(sse(vec![
        ev_response_created(&format!("resp-{id}")),
        ev_assistant_message(&format!("msg-{id}"), message),
        ev_completed(&format!("resp-{id}")),
    ]))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn exact_retry_uses_a_fresh_child_and_skip_settles_null_after_cleanup() -> Result<()> {
    let server = responses::start_mock_server().await;
    let router = ControlRouter::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(router.clone())
        .mount(&server)
        .await;

    let test = test_codex()
        .with_pre_build_hook(|codex_home| {
            let workflows = codex_home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow root");
            std::fs::write(
                workflows.join("selected-agent-controls.workflow.js"),
                WORKFLOW_SOURCE,
            )
            .expect("write workflow fixture");
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow");
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("use in-process host");
            // Starting the second generation and then the second logical agent proves each
            // controlled attempt releases ordinary scheduler admission before continuing.
            // The root thread occupies one slot, leaving exactly one child slot.
            config.multi_agent_v2.max_concurrent_threads_per_session = 2;
        })
        .build_with_auto_env(&server)
        .await?;
    let mut receiver = test.codex.subscribe_events();
    let run_id = test
        .codex
        .start_saved_workflow(WORKFLOW_NAME, json!({}))
        .await?;

    let mut labels = HashMap::<u64, String>::new();
    let mut events = Vec::new();
    let mut retry_children = Vec::new();
    let mut retried_node = None;
    let mut skipped_node = None;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(20), receiver.recv())
            .await
            .with_context(|| {
                format!(
                    "timed out waiting for selected-control workflow; requests: {}; events: {events:#?}",
                    *router.requests.lock().unwrap(),
                )
            })??;
        let EventMsg::Workflow(event) = event.msg else {
            continue;
        };
        if workflow_event_run_id(&event) != run_id {
            continue;
        }
        match &event {
            WorkflowEvent::AgentBegin(begin) => {
                labels.insert(begin.node_id, begin.label.clone());
            }
            WorkflowEvent::AgentBound(bound) => {
                match labels.get(&bound.node_id).map(String::as_str) {
                    Some("retry-target") if bound.attempt == 0 => {
                        retried_node = Some(bound.node_id);
                        retry_children.push(bound.child_thread_id.clone());
                        let disposition = tokio::time::timeout(
                            Duration::from_secs(10),
                            test.codex.control_workflow_agent(
                                &run_id,
                                bound.node_id,
                                bound.attempt,
                                WorkflowAgentControlAction::Retry,
                            ),
                        )
                        .await
                        .context("retry cleanup barrier timed out")?;
                        assert_eq!(
                            disposition,
                            WorkflowAgentControlDisposition::RetryScheduled { attempt: 1 }
                        );
                    }
                    Some("retry-target") if bound.attempt == 1 => {
                        retry_children.push(bound.child_thread_id.clone());
                        assert_eq!(
                            test.codex
                                .control_workflow_agent(
                                    &run_id,
                                    bound.node_id,
                                    0,
                                    WorkflowAgentControlAction::Skip,
                                )
                                .await,
                            WorkflowAgentControlDisposition::Unavailable,
                            "stale attempt zero must not cancel the fresh generation",
                        );
                    }
                    Some("skip-target") if bound.attempt == 0 => {
                        skipped_node = Some(bound.node_id);
                        let disposition = tokio::time::timeout(
                            Duration::from_secs(10),
                            test.codex.control_workflow_agent(
                                &run_id,
                                bound.node_id,
                                bound.attempt,
                                WorkflowAgentControlAction::Skip,
                            ),
                        )
                        .await
                        .context("skip cleanup/journal barrier timed out")?;
                        assert_eq!(disposition, WorkflowAgentControlDisposition::Skipped);
                    }
                    _ => {}
                }
            }
            WorkflowEvent::RunEnd(_) => {
                events.push(event);
                break;
            }
            _ => {}
        }
        events.push(event);
    }

    let retried_node =
        retried_node.with_context(|| format!("retry node observed; events: {events:#?}"))?;
    let skipped_node =
        skipped_node.with_context(|| format!("skip node observed; events: {events:#?}"))?;
    assert_eq!(retry_children.len(), 2);
    assert_ne!(retry_children[0], retry_children[1]);
    let retry_ends = events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::AgentEnd(end) if end.node_id == retried_node => Some(end),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(retry_ends.len(), 1, "retry has one logical terminal event");
    assert_eq!(retry_ends[0].attempt, 1);
    assert_eq!(
        retry_ends[0].last_attempt_reason,
        Some(WorkflowAgentAttemptReason::UserRetry)
    );
    assert!(matches!(retry_ends[0].status, AgentStatus::Completed(_)));
    let skip_end = events
        .iter()
        .find_map(|event| match event {
            WorkflowEvent::AgentEnd(end) if end.node_id == skipped_node => Some(end),
            _ => None,
        })
        .context("skip terminal event")?;
    assert_eq!(skip_end.attempt, 0);
    assert_eq!(
        skip_end.last_attempt_reason,
        Some(WorkflowAgentAttemptReason::UserSkip)
    );
    assert_eq!(skip_end.status, AgentStatus::Shutdown);
    assert!(skip_end.returned_null);
    let logs = events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::Log(log) => Some(log.message.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        logs,
        vec!["CONTROL_RESULT:{\"retried\":\"retry-ok\",\"skipped\":null}"]
    );

    let paths = WorkflowRunPaths::new(test.codex_home_path(), &run_id);
    let replay = ReplayJournal::load(&paths.journal())?;
    assert_eq!(replay.len(), 2, "one terminal anchor per logical call");
    let retry = replay.lookup(0).context("retry replay anchor")?;
    assert_eq!(retry.attempt, 1);
    assert_eq!(retry.status, Some(JournalAgentStatus::Completed));
    assert_eq!(retry.control_reason, None);
    let skip = replay.lookup(1).context("skip replay anchor")?;
    assert_eq!(skip.attempt, 0);
    assert_eq!(skip.status, Some(JournalAgentStatus::Completed));
    assert_eq!(skip.control_reason, Some(AgentControlReason::UserSkip));
    assert!(skip.ret.is_null());
    let links = RunAgentJournal::load(&paths.journal())?;
    assert_eq!(
        links
            .links()
            .iter()
            .map(|link| (link.ordinal, link.attempt))
            .collect::<Vec<_>>(),
        vec![(0, 0), (0, 1), (1, 0)]
    );
    assert!(
        *router.requests.lock().unwrap() >= 1,
        "the fresh retry child must run a model turn"
    );

    test.codex.shutdown_and_wait().await?;
    Ok(())
}

fn workflow_event_run_id(event: &WorkflowEvent) -> &str {
    match event {
        WorkflowEvent::RunBegin(event) => &event.run_id,
        WorkflowEvent::RunEnd(event) => &event.run_id,
        WorkflowEvent::PhaseBegin(event) => &event.run_id,
        WorkflowEvent::PhaseEnd(event) => &event.run_id,
        WorkflowEvent::GroupBegin(event) => &event.run_id,
        WorkflowEvent::GroupEnd(event) => &event.run_id,
        WorkflowEvent::AgentBegin(event) => &event.run_id,
        WorkflowEvent::AgentBound(event) => &event.run_id,
        WorkflowEvent::AgentUpdated(event) => &event.run_id,
        WorkflowEvent::AgentEnd(event) => &event.run_id,
        WorkflowEvent::Log(event) => &event.run_id,
    }
}

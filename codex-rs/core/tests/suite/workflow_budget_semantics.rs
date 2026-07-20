#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Real-stack coverage for unmetered versus explicitly zero-limited workflow budgets.

use std::io::Cursor;

use anyhow::Result;
use codex_core::workflow_cli::RunWatchBudget;
use codex_features::Feature;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::WorkflowEvent;
use codex_workflow_journal::storage::WorkflowRunPaths;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const CALL_ID: &str = "call-budget-workflow";
const CHILD_MARKER: &str = "BUDGET_SEMANTICS_CHILD";

struct BudgetRouter {
    workflow_name: &'static str,
    workflow_args: Value,
}

impl Respond for BudgetRouter {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = request_body_json(request);
        if body.to_string().contains(CHILD_MARKER) {
            return sse_response(sse(vec![
                ev_response_created("resp-budget-child"),
                ev_assistant_message("msg-budget-child", "budget work complete"),
                completed_with_output_tokens("resp-budget-child", 7),
            ]));
        }
        if contains_call_output(&body) {
            return sse_response(sse(vec![
                ev_response_created("resp-budget-parent-close"),
                ev_assistant_message("msg-budget-parent-close", "workflow admitted"),
                ev_completed("resp-budget-parent-close"),
            ]));
        }
        let arguments = json!({
            "name": self.workflow_name,
            "args": self.workflow_args,
        })
        .to_string();
        sse_response(sse(vec![
            ev_response_created("resp-budget-parent-open"),
            ev_function_call(CALL_ID, "workflow_run", &arguments),
            ev_completed("resp-budget-parent-open"),
        ]))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn real_workflows_preserve_unmetered_and_explicit_zero_budget_semantics() -> Result<()> {
    let unmetered = run_case(
        "budget-unmetered",
        r#"export const meta = { name: 'budget-unmetered', description: 'unmetered budget' };
const result = await agent('BUDGET_SEMANTICS_CHILD');
text(result);
"#,
        json!({}),
        None,
    )
    .await?;
    assert!(
        unmetered.spent > 0,
        "fixture child must produce metered usage"
    );

    let zero_limited = run_case(
        "budget-zero",
        r#"export const meta = { name: 'budget-zero', description: 'zero budget' };
text('no child needed');
"#,
        json!({"budget": {"total": 0}}),
        Some(0),
    )
    .await?;
    assert_eq!(
        zero_limited,
        RunWatchBudget {
            spent: 0,
            total: Some(0)
        }
    );
    Ok(())
}

async fn run_case(
    workflow_name: &'static str,
    workflow_source: &'static str,
    workflow_args: Value,
    expected_total: Option<i64>,
) -> Result<RunWatchBudget> {
    let server = responses::start_mock_server().await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(BudgetRouter {
            workflow_name,
            workflow_args,
        })
        .mount(&server)
        .await;

    let server_uri = server.uri();
    let test = test_codex()
        .with_model("gpt-5.5")
        .with_pre_build_hook(move |home| {
            std::fs::write(
                home.join("config.toml"),
                format!("openai_base_url = \"{server_uri}/v1\"\n"),
            )
            .expect("write fixture provider config");
            let workflows = home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow registry");
            std::fs::write(
                workflows.join(format!("{workflow_name}.workflow.js")),
                workflow_source,
            )
            .expect("write workflow fixture");
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
            config.rollout_budget = None;
        })
        .build_with_auto_env(&server)
        .await?;

    let mut events = test.codex.subscribe_events();
    test.submit_turn("run the budget semantics fixture").await?;
    let terminal = loop {
        let event = events.recv().await?;
        if let EventMsg::Workflow(WorkflowEvent::RunEnd(terminal)) = event.msg {
            break terminal;
        }
    };
    assert_eq!(terminal.status, AgentStatus::Completed(None));
    assert_eq!(terminal.total, expected_total);

    let paths = WorkflowRunPaths::new(test.codex_home_path(), &terminal.run_id);
    let meta = paths.read_meta_bounded()?;
    assert_eq!(meta.budget_total.map(|total| total as i64), expected_total);
    let view = codex_core::workflow_cli::inspect_run(&test.config, &terminal.run_id).await?;
    let budget = view.budget.expect("terminal progress budget");
    assert_eq!(
        budget,
        RunWatchBudget {
            spent: terminal.spent,
            total: expected_total
        }
    );

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    test.codex.wait_until_terminated().await;
    Ok(budget)
}

fn request_body_json(request: &wiremock::Request) -> Value {
    let compressed = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.split(',').any(|part| part.trim() == "zstd"));
    let bytes = if compressed {
        zstd::stream::decode_all(Cursor::new(request.body.as_slice()))
            .expect("decode compressed request")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).expect("request body is JSON")
}

fn contains_call_output(body: &Value) -> bool {
    body["input"].as_array().into_iter().flatten().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(CALL_ID)
    })
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

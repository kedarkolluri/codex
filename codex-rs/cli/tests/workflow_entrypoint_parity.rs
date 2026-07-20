#![allow(clippy::expect_used)]
//! UAT-9: normalized durable parity across the three Dynamic Workflow entrypoints.
//!
//! `/workflow` is intentionally a thin TUI adapter: it parses the visible command/picker choice
//! and sends app-server `workflow/start`. Therefore the real app-server process is the deterministic
//! semantic twin for the slash entrypoint in this headless gate. Literal keyboard/picker coverage
//! remains in the PTY UAT lane. The other lanes drive the assembled `codex workflow run` binary and
//! a model-emitted `workflow_run` call through a real app-server turn. All three execute the same
//! saved script and args, then compare stable workflow identity, argument digests, final event
//! projection, outcome metadata, and ordered journal records. UUIDs and host timestamps are the
//! only normalized fields; concurrent scheduling is not part of this narration-only fixture.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use app_test_support::TestAppServer;
use app_test_support::to_response;
use app_test_support::write_mock_responses_config_toml;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_app_server_protocol::WorkflowStartParams;
use codex_app_server_protocol::WorkflowStartResponse;
use codex_app_server_protocol::WorkflowStartedNotification;
use codex_features::Feature;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
const WORKFLOW_NAME: &str = "uat9-entrypoint-parity";
const WORKFLOW_CALL_ID: &str = "call-uat9-workflow";
const WORKFLOW_ARGS: &str = r#"{"value":"same","nested":{"n":1}}"#;
const WORKFLOW_SOURCE: &str = r#"export const meta = {
  name: 'uat9-entrypoint-parity',
  description: 'normalized three-entrypoint parity',
  phases: ['prepare', 'finish'],
};
phase('prepare');
log('uat9 value=' + args.value);
phase('finish');
log('uat9 complete');
text('uat9-output:' + args.value);
"#;

#[derive(Debug, PartialEq)]
struct NormalizedRun {
    journal: Vec<Value>,
    meta: Value,
    progress: Value,
    script: String,
}

#[derive(Clone, Default)]
struct ModelToolResponder {
    request_count: Arc<AtomicUsize>,
}

impl Respond for ModelToolResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let ordinal = self.request_count.fetch_add(1, Ordering::SeqCst);
        let body = if ordinal == 0 {
            workflow_tool_sse()
        } else {
            final_message_sse()
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(body, "text/event-stream")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat9_cli_model_tool_and_slash_twin_have_normalized_durable_parity() -> Result<()> {
    let server = MockServer::start().await;
    let responder = ModelToolResponder::default();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let cli = run_cli_lane(&server).await?;
    let slash = run_slash_twin_lane(&server).await?;
    let model_tool = run_model_tool_lane(&server).await?;

    assert_eq!(slash, cli, "app-server workflow/start must match the CLI");
    assert_eq!(
        model_tool, cli,
        "model workflow_run must match CLI and slash semantics"
    );
    assert_eq!(
        responder.request_count.load(Ordering::SeqCst),
        2,
        "only the model-tool lane should call the fixture model"
    );
    Ok(())
}

async fn run_cli_lane(server: &MockServer) -> Result<NormalizedRun> {
    let home = TempDir::new()?;
    write_lane_config_and_workflow(home.path(), server)?;
    let output = tokio::process::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?)
        .env("CODEX_HOME", home.path())
        .args([
            "--enable",
            "workflow",
            "workflow",
            "run",
            WORKFLOW_NAME,
            "--args",
            WORKFLOW_ARGS,
        ])
        .output()
        .await?;
    anyhow::ensure!(
        output.status.success(),
        "CLI lane failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout)?;
    anyhow::ensure!(stdout.contains("uat9-output:same"), "CLI output: {stdout}");
    let run_id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("run ID: "))
        .context("CLI lane did not print a run ID")?;
    read_normalized_run(home.path(), run_id).await
}

async fn run_slash_twin_lane(server: &MockServer) -> Result<NormalizedRun> {
    let home = TempDir::new()?;
    write_lane_config_and_workflow(home.path(), server)?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, app.initialize()).await??;
    let thread_id = start_thread(&mut app).await?;

    let request_id = app
        .send_workflow_start_request(WorkflowStartParams {
            thread_id,
            name: WORKFLOW_NAME.to_string(),
            args: Some(serde_json::from_str(WORKFLOW_ARGS)?),
        })
        .await?;
    let response = timeout(
        DEFAULT_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    let response = to_response::<WorkflowStartResponse>(response)?;
    wait_for_workflow_completed(&mut app, &response.run_id).await?;
    read_normalized_run(home.path(), &response.run_id).await
}

async fn run_model_tool_lane(server: &MockServer) -> Result<NormalizedRun> {
    let home = TempDir::new()?;
    write_lane_config_and_workflow(home.path(), server)?;
    let mut app = TestAppServer::builder()
        .with_codex_home(home.path())
        .build()
        .await?;
    timeout(DEFAULT_TIMEOUT, app.initialize()).await??;
    let thread_id = start_thread(&mut app).await?;

    let request_id = app
        .send_turn_start_request(TurnStartParams {
            thread_id,
            input: vec![V2UserInput::Text {
                text: "start the saved UAT-9 workflow".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    timeout(
        DEFAULT_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;

    let started = timeout(
        DEFAULT_TIMEOUT,
        app.read_stream_until_notification_message("workflow/started"),
    )
    .await??;
    let started: WorkflowStartedNotification = serde_json::from_value(
        started
            .params
            .context("workflow/started notification must carry params")?,
    )?;
    assert_eq!(started.name, WORKFLOW_NAME);
    wait_for_workflow_completed(&mut app, &started.run_id).await?;
    timeout(
        DEFAULT_TIMEOUT,
        app.read_stream_until_notification_message("turn/completed"),
    )
    .await??;
    read_normalized_run(home.path(), &started.run_id).await
}

async fn start_thread(app: &mut TestAppServer) -> Result<String> {
    let request_id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let response = timeout(
        DEFAULT_TIMEOUT,
        app.read_stream_until_response_message(RequestId::Integer(request_id)),
    )
    .await??;
    Ok(to_response::<ThreadStartResponse>(response)?.thread.id)
}

async fn wait_for_workflow_completed(app: &mut TestAppServer, run_id: &str) -> Result<()> {
    timeout(
        DEFAULT_TIMEOUT,
        app.read_stream_until_matching_notification(
            "matching workflow/completed",
            |notification| {
                notification.method == "workflow/completed"
                    && notification
                        .params
                        .as_ref()
                        .and_then(|params| params.get("runId"))
                        .and_then(Value::as_str)
                        == Some(run_id)
            },
        ),
    )
    .await??;
    Ok(())
}

fn write_lane_config_and_workflow(home: &Path, server: &MockServer) -> Result<()> {
    let features = BTreeMap::from([(Feature::Workflow, true)]);
    write_mock_responses_config_toml(
        home,
        &server.uri(),
        &features,
        100_000,
        /*requires_openai_auth*/ None,
        "mock_provider",
        "compact",
    )?;
    let workflows = home.join("workflows");
    std::fs::create_dir_all(&workflows)?;
    std::fs::write(
        workflows.join("uat9-entrypoint-parity.workflow.js"),
        WORKFLOW_SOURCE,
    )?;
    Ok(())
}

async fn read_normalized_run(home: &Path, run_id: &str) -> Result<NormalizedRun> {
    let run_dir = home.join("workflows/runs").join(run_id);
    let deadline = tokio::time::Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if let Ok(meta) = std::fs::read_to_string(run_dir.join("meta.json"))
            && let Ok(meta) = serde_json::from_str::<Value>(&meta)
            && meta["status"] == "completed"
        {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for terminal metadata for {run_id}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl"))?
        .lines()
        .filter(|line| !line.is_empty())
        .map(serde_json::from_str::<Value>)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .map(normalize_journal_line)
        .collect();
    let meta = normalize_meta(serde_json::from_slice(&std::fs::read(
        run_dir.join("meta.json"),
    )?)?);
    let progress = normalize_progress(serde_json::from_slice(&std::fs::read(
        run_dir.join("progress.json"),
    )?)?);
    let script = std::fs::read_to_string(run_dir.join("script.js"))?;
    Ok(NormalizedRun {
        journal,
        meta,
        progress,
        script,
    })
}

fn normalize_journal_line(mut line: Value) -> Value {
    let object = line.as_object_mut().expect("journal line is an object");
    object.remove("timestamp");
    if object.get("type") == Some(&json!("run_meta")) {
        object.insert("run_id".to_string(), json!("<run>"));
        object.insert("owner_thread_id".to_string(), json!("<owner>"));
        object.remove("created_at");
    }
    line
}

fn normalize_meta(mut meta: Value) -> Value {
    let object = meta.as_object_mut().expect("run metadata is an object");
    object.insert("run_id".to_string(), json!("<run>"));
    object.insert("owner_thread_id".to_string(), json!("<owner>"));
    object.remove("created_at");
    meta
}

fn normalize_progress(mut progress: Value) -> Value {
    progress
        .as_object_mut()
        .expect("progress snapshot is an object")
        .insert("run_id".to_string(), json!("<run>"));
    progress
}

fn workflow_tool_sse() -> String {
    let arguments = json!({
        "name": WORKFLOW_NAME,
        "args": serde_json::from_str::<Value>(WORKFLOW_ARGS).expect("valid fixture args"),
    })
    .to_string();
    response_sse(vec![
        json!({
            "type": "response.created",
            "response": {"id": "resp-uat9-open"},
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "function_call",
                "id": "item-uat9-workflow",
                "call_id": WORKFLOW_CALL_ID,
                "name": "workflow_run",
                "arguments": arguments,
            },
        }),
        json!({
            "type": "response.completed",
            "response": {"id": "resp-uat9-open", "usage": zero_usage()},
        }),
    ])
}

fn final_message_sse() -> String {
    response_sse(vec![
        json!({
            "type": "response.created",
            "response": {"id": "resp-uat9-close"},
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": "msg-uat9-close",
                "content": [{"type": "output_text", "text": "workflow admitted"}],
            },
        }),
        json!({
            "type": "response.completed",
            "response": {"id": "resp-uat9-close", "usage": zero_usage()},
        }),
    ])
}

fn zero_usage() -> Value {
    json!({
        "input_tokens": 0,
        "input_tokens_details": null,
        "output_tokens": 0,
        "output_tokens_details": null,
        "total_tokens": 0,
    })
}

fn response_sse(events: Vec<Value>) -> String {
    events
        .into_iter()
        .map(|event| {
            let kind = event["type"].as_str().expect("event type");
            format!("event: {kind}\ndata: {event}\n\n")
        })
        .collect()
}

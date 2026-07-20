//! Product-level UAT for watching a workflow while its process-owned host is live.
//!
//! The fixture router holds both child model round-trips behind explicit gates. This lets the
//! assembled `codex workflow watch --json` process observe the initially bound child, a live
//! tool/token counter update, and the terminal frame without relying on response delays.

#![allow(clippy::expect_used)]

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_protocol::ThreadId;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio::process::Child;
use tokio::process::Command;
use tokio::sync::Notify;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);
const PROFILE: &str = "live-watch";
const MODEL: &str = "fixture-live-watch-model";
const TOOL_CALL_ID: &str = "call-live-watch-tool";
const TOOL_MARKER: &str = "LIVE_WATCH_TOOL_OK";
const BUDGET_TOTAL: i64 = 10_000;
const WORKFLOW_SOURCE: &str = r#"export const meta = {
  name: 'live-watch',
  description: 'real live workflow watch UAT',
  phases: ['observe'],
};
phase('observe');
const result = await agent('run the live watch tool fixture', { label: 'watched-child' });
text(result);
"#;

#[derive(Default)]
struct Gate {
    released: Mutex<bool>,
    released_signal: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut released = self.released.lock().expect("gate lock");
        while !*released {
            released = self.released_signal.wait(released).expect("gate wait");
        }
    }

    fn release(&self) {
        *self.released.lock().expect("gate lock") = true;
        self.released_signal.notify_all();
    }
}

struct RouterState {
    stage: AtomicU8,
    stage_changed: Notify,
    initial_response: Gate,
    terminal_response: Gate,
    tool_output_seen: AtomicBool,
}

impl Default for RouterState {
    fn default() -> Self {
        Self {
            stage: AtomicU8::new(0),
            stage_changed: Notify::new(),
            initial_response: Gate::default(),
            terminal_response: Gate::default(),
            tool_output_seen: AtomicBool::new(false),
        }
    }
}

#[derive(Clone)]
struct GatedRouter {
    state: Arc<RouterState>,
}

impl Respond for GatedRouter {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value =
            serde_json::from_slice(&request.body).expect("responses request must be JSON");
        if contains_function_output(&body, TOOL_CALL_ID) {
            self.state
                .tool_output_seen
                .store(body.to_string().contains(TOOL_MARKER), Ordering::SeqCst);
            self.state.stage.store(2, Ordering::SeqCst);
            self.state.stage_changed.notify_one();
            self.state.terminal_response.wait();
            sse_response(vec![
                response_created("resp-live-watch-final"),
                assistant_message("msg-live-watch-final", "live-watch-finished"),
                response_completed("resp-live-watch-final", /*output_tokens*/ 60),
            ])
        } else {
            self.state.stage.store(1, Ordering::SeqCst);
            self.state.stage_changed.notify_one();
            self.state.initial_response.wait();
            sse_response(vec![
                response_created("resp-live-watch-tool"),
                function_call(TOOL_CALL_ID, "shell_command", &tool_arguments().to_string()),
                response_completed("resp-live-watch-tool", /*output_tokens*/ 40),
            ])
        }
    }
}

struct RouterController {
    state: Arc<RouterState>,
}

impl RouterController {
    fn pair() -> (Self, GatedRouter) {
        let state = Arc::new(RouterState::default());
        (
            Self {
                state: Arc::clone(&state),
            },
            GatedRouter { state },
        )
    }

    async fn wait_for_stage(&self, expected: u8) -> Result<()> {
        timeout(DEFAULT_TIMEOUT, async {
            while self.state.stage.load(Ordering::SeqCst) < expected {
                self.state.stage_changed.notified().await;
            }
        })
        .await
        .with_context(|| format!("router did not reach stage {expected}"))?;
        Ok(())
    }

    fn release_initial(&self) {
        self.state.initial_response.release();
    }

    fn release_terminal(&self) {
        self.state.terminal_response.release();
    }
}

impl Drop for RouterController {
    fn drop(&mut self) {
        self.release_initial();
        self.release_terminal();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn workflow_watch_streams_monotonic_live_and_terminal_ndjson_frames() -> Result<()> {
    let server = MockServer::start().await;
    let (router, responder) = RouterController::pair();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(responder)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    std::fs::write(workspace.path().join("live-watch.js"), WORKFLOW_SOURCE)?;
    write_profile_config(codex_home.path(), &server)?;

    let mut run_command = base_command(codex_home.path(), workspace.path())?;
    run_command
        .args([
            "workflow",
            "run",
            "live-watch.js",
            "--args",
            r#"{"budget":{"total":10000}}"#,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let run_child = run_command.spawn()?;

    router.wait_for_stage(/*expected*/ 1).await?;
    let run_id = only_run_id(codex_home.path())?;
    let mut watch_command = base_command(codex_home.path(), workspace.path())?;
    watch_command
        .args(["workflow", "watch", &run_id, "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut watch_child = watch_command.spawn()?;
    let watch_stdout = watch_child.stdout.take().context("watch stdout pipe")?;
    let mut watch_lines = BufReader::new(watch_stdout).lines();

    let mut frames = Vec::new();
    let initial = next_frame(&mut watch_lines).await?;
    let child_thread_id = required_child_thread_id(&initial)?;
    assert_eq!(
        frame_projection(&initial, &child_thread_id)?,
        expected_projection(
            &run_id, "running", /*terminal*/ false, "active", /*total_tokens*/ 0,
            /*tool_call_count*/ 0, /*budget*/ None,
        ),
    );
    frames.push(initial);

    router.release_initial();
    router.wait_for_stage(/*expected*/ 2).await?;
    let live = read_until_frame(&mut watch_lines, &mut frames, |frame| {
        agent_counter(frame, "totalTokens") == Some(40)
            && agent_counter(frame, "toolCallCount") == Some(1)
    })
    .await?;
    assert_eq!(
        frame_projection(&live, &child_thread_id)?,
        expected_projection(
            &run_id, "running", /*terminal*/ false, "active", /*total_tokens*/ 40,
            /*tool_call_count*/ 1, /*budget*/ None,
        ),
    );

    router.release_terminal();
    let terminal = read_until_frame(&mut watch_lines, &mut frames, |frame| {
        frame["terminal"] == true
    })
    .await?;
    assert_eq!(
        frame_projection(&terminal, &child_thread_id)?,
        expected_projection(
            &run_id,
            "completed",
            /*terminal*/ true,
            "completed",
            /*total_tokens*/ 100,
            /*tool_call_count*/ 1,
            /*budget*/ Some((100, BUDGET_TOTAL)),
        ),
    );

    assert_monotonic_frames(&frames, &run_id, &child_thread_id)?;
    assert!(
        frames.len() >= 3,
        "watch must emit multiple live frames and a terminal frame"
    );
    assert!(
        router.state.tool_output_seen.load(Ordering::SeqCst),
        "the second model round-trip must contain the real tool's success output",
    );

    let watch_output = wait_for_child(watch_child, "workflow watch").await?;
    assert!(
        watch_output.status.success(),
        "workflow watch failed: {watch_output:?}"
    );
    let run_output = wait_for_child(run_child, "workflow run").await?;
    assert!(
        run_output.status.success(),
        "workflow run failed: {run_output:?}"
    );
    assert!(
        String::from_utf8(run_output.stdout)?.contains("live-watch-finished"),
        "workflow result must come from the live child",
    );

    Ok(())
}

async fn next_frame(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> Result<Value> {
    let line = timeout(DEFAULT_TIMEOUT, lines.next_line())
        .await
        .context("timed out waiting for workflow watch frame")??
        .context("workflow watch closed before its terminal frame")?;
    serde_json::from_str(&line).context("workflow watch emitted invalid NDJSON")
}

async fn read_until_frame(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    frames: &mut Vec<Value>,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value> {
    loop {
        let frame = next_frame(lines).await?;
        let matched = predicate(&frame);
        frames.push(frame.clone());
        if matched {
            return Ok(frame);
        }
    }
}

fn frame_projection(frame: &Value, child_thread_id: &str) -> Result<Value> {
    let node = only_agent(frame)?;
    anyhow::ensure!(
        node["childThreadId"] == child_thread_id,
        "child linkage changed"
    );
    Ok(json!({
        "runId": frame["runId"],
        "name": frame["name"],
        "status": frame["status"],
        "terminal": frame["terminal"],
        "budget": frame["budget"],
        "phases": frame["phases"],
        "agent": {
            "label": node["label"],
            "model": node["model"],
            "effort": node["effort"],
            "childThreadId": "<child>",
            "status": node["status"],
            "totalTokens": node["totalTokens"],
            "toolCallCount": node["toolCallCount"],
            "returnedNull": node["returnedNull"],
        },
        "warnings": frame["warnings"],
    }))
}

fn expected_projection(
    run_id: &str,
    status: &str,
    terminal: bool,
    phase_status: &str,
    total_tokens: i64,
    tool_call_count: u64,
    budget: Option<(i64, i64)>,
) -> Value {
    json!({
        "runId": run_id,
        "name": "live-watch",
        "status": status,
        "terminal": terminal,
        "budget": budget.map(|(spent, total)| json!({"spent": spent, "total": total})),
        "phases": [{"index": 0, "title": "observe", "status": phase_status, "implicit": false}],
        "agent": {
            "label": "watched-child",
            "model": MODEL,
            "effort": "medium",
            "childThreadId": "<child>",
            "status": status,
            "totalTokens": total_tokens,
            "toolCallCount": tool_call_count,
            "returnedNull": false,
        },
        "warnings": [],
    })
}

fn assert_monotonic_frames(frames: &[Value], run_id: &str, child_thread_id: &str) -> Result<()> {
    let mut previous_tokens = 0;
    let mut previous_tools = 0;
    let mut previous_phase = 0;
    for (index, frame) in frames.iter().enumerate() {
        assert_eq!(frame["runId"], run_id);
        assert_eq!(required_child_thread_id(frame)?, child_thread_id);
        let tokens = agent_counter(frame, "totalTokens").context("agent totalTokens")?;
        let tools = agent_counter(frame, "toolCallCount").context("agent toolCallCount")?;
        let phase = match frame["phases"][0]["status"].as_str() {
            Some("pending") => 0,
            Some("active") => 1,
            Some("completed") => 2,
            other => anyhow::bail!("unexpected phase status: {other:?}"),
        };
        anyhow::ensure!(
            tokens >= previous_tokens,
            "token counter regressed at frame {index}"
        );
        anyhow::ensure!(
            tools >= previous_tools,
            "tool counter regressed at frame {index}"
        );
        anyhow::ensure!(
            phase >= previous_phase,
            "phase state regressed at frame {index}"
        );
        anyhow::ensure!(
            index + 1 == frames.len() || frame["terminal"] == false,
            "terminal frame was not last"
        );
        previous_tokens = tokens;
        previous_tools = tools;
        previous_phase = phase;
    }
    Ok(())
}

fn only_agent(frame: &Value) -> Result<&Value> {
    let agents = frame["nodes"]
        .as_array()
        .context("watch nodes")?
        .iter()
        .filter(|node| node["kind"] == "agent")
        .collect::<Vec<_>>();
    anyhow::ensure!(agents.len() == 1, "expected one watched agent: {agents:?}");
    Ok(agents[0])
}

fn required_child_thread_id(frame: &Value) -> Result<String> {
    let thread_id = only_agent(frame)?["childThreadId"]
        .as_str()
        .context("watch frame has no child thread linkage")?;
    ThreadId::from_string(thread_id).context("watch frame child linkage is not a thread ID")?;
    Ok(thread_id.to_string())
}

fn agent_counter(frame: &Value, field: &str) -> Option<i64> {
    only_agent(frame).ok()?[field].as_i64()
}

fn only_run_id(codex_home: &Path) -> Result<String> {
    let runs_root = codex_home.join("workflows/runs");
    let run_ids = std::fs::read_dir(&runs_root)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect::<Vec<_>>();
    anyhow::ensure!(
        run_ids.len() == 1,
        "expected exactly one live run: {run_ids:?}"
    );
    Ok(run_ids[0].clone())
}

fn base_command(codex_home: &Path, workspace: &Path) -> Result<Command> {
    let mut command = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home)
        .env(
            "CODEX_CODE_MODE_HOST_PATH",
            codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?,
        )
        .args(["--profile", PROFILE, "-C"])
        .arg(workspace)
        .kill_on_drop(true);
    Ok(command)
}

async fn wait_for_child(child: Child, description: &str) -> Result<std::process::Output> {
    timeout(DEFAULT_TIMEOUT, child.wait_with_output())
        .await
        .with_context(|| format!("timed out waiting for {description}"))?
        .map_err(Into::into)
}

fn write_profile_config(codex_home: &Path, server: &MockServer) -> Result<()> {
    std::fs::write(
        codex_home.join(format!("{PROFILE}.config.toml")),
        format!(
            r#"
model = "{MODEL}"
model_provider = "fixture_router"
model_reasoning_effort = "medium"
approval_policy = "never"
sandbox_mode = "read-only"

[features]
workflow = true

[model_providers.fixture_router]
name = "Live watch fixture router"
base_url = "{}/v1"
wire_api = "responses"
request_max_retries = 0
stream_max_retries = 0
supports_websockets = false
"#,
            server.uri()
        ),
    )?;
    Ok(())
}

fn contains_function_output(body: &Value, call_id: &str) -> bool {
    body["input"].as_array().into_iter().flatten().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
    })
}

fn sse_response(events: Vec<Value>) -> ResponseTemplate {
    let body = events
        .into_iter()
        .map(|event| {
            let event_type = event["type"].as_str().expect("event type");
            format!("event: {event_type}\ndata: {event}\n\n")
        })
        .collect::<String>();
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_raw(body, "text/event-stream")
}

fn response_created(id: &str) -> Value {
    json!({"type": "response.created", "response": {"id": id}})
}

fn response_completed(id: &str, output_tokens: i64) -> Value {
    json!({
        "type": "response.completed",
        "response": {
            "id": id,
            "usage": {
                "input_tokens": 0,
                "input_tokens_details": null,
                "output_tokens": output_tokens,
                "output_tokens_details": null,
                "total_tokens": output_tokens,
            },
        },
    })
}

fn assistant_message(id: &str, text: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "item": {
            "type": "message",
            "role": "assistant",
            "id": id,
            "content": [{"type": "output_text", "text": text}],
        },
    })
}

fn function_call(call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "type": "response.output_item.done",
        "item": {
            "type": "function_call",
            "call_id": call_id,
            "name": name,
            "arguments": arguments,
        },
    })
}

#[cfg(windows)]
fn tool_arguments() -> Value {
    json!({
        "command": format!("Write-Output {TOOL_MARKER}"),
        "login": false,
        "timeout_ms": 10_000,
    })
}

#[cfg(not(windows))]
fn tool_arguments() -> Value {
    json!({
        "command": format!("printf {TOOL_MARKER}"),
        "login": false,
        "timeout_ms": 10_000,
    })
}

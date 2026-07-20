//! Product-level UAT for resuming a genuinely interrupted workflow prefix.
//!
//! The source run is an assembled `codex workflow run` process using the default
//! process-owned code-mode host. Its first child completes, its second child is
//! held at the fixture provider, and only that spawned CLI process is killed.
//! The resumed CLI must reuse the one durable completed ordinal and execute the
//! remaining tail live under a fresh run identity. A final lane changes the
//! inherited model to prove that the run-level execution fingerprint disables
//! replay even when source and args are unchanged.

#![allow(clippy::expect_used)]

use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use tokio::process::Child;
use tokio::process::Command;
use tokio::time::Instant;
use tokio::time::sleep;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(90);
const STALLED_RESPONSE_DELAY: Duration = Duration::from_secs(120);
const PROFILE: &str = "partial-resume";
const ORIGINAL_MODEL: &str = "fixture-model-v1";
const CHANGED_MODEL: &str = "fixture-model-v2";
const BUDGET_TOTAL: i64 = 10_000;
const WORKFLOW_ARGS: &str = r#"{"budget":{"total":10000},"tag":"unchanged"}"#;
const WORKFLOW_SOURCE: &str = r#"export const meta = {
  name: 'partial-prefix-resume',
  description: 'real interrupted prefix resume UAT',
  phases: ['agents'],
};
phase('agents');
const first = await agent('first', { label: 'first' });
const firstSpent = budget.spent();
const second = await agent('second', { label: 'second' });
const third = await agent('third', { label: 'third' });
text(JSON.stringify({
  results: [first, second, third],
  firstSpent,
  spent: budget.spent(),
  total: budget.total,
  remaining: budget.remaining(),
}));
"#;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservedRequest {
    prompt: String,
    model: String,
    stalled: bool,
}

#[derive(Clone)]
struct ControlledAgentResponder {
    requests: Arc<Mutex<Vec<ObservedRequest>>>,
    stall_first_second: Arc<AtomicBool>,
}

impl Default for ControlledAgentResponder {
    fn default() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            stall_first_second: Arc::new(AtomicBool::new(true)),
        }
    }
}

impl ControlledAgentResponder {
    fn snapshot(&self) -> Vec<ObservedRequest> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Respond for ControlledAgentResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("responses request JSON");
        let prompt = last_user_text(&body).unwrap_or("<missing>").to_string();
        let model = body["model"].as_str().unwrap_or("<missing>").to_string();
        let stalled = prompt == "second" && self.stall_first_second.swap(false, Ordering::SeqCst);
        let sequence = {
            let mut requests = self.requests.lock().expect("requests lock");
            let sequence = requests.len();
            requests.push(ObservedRequest {
                prompt: prompt.clone(),
                model,
                stalled,
            });
            sequence
        };

        let Some((answer, output_tokens)) = fixture_answer(&prompt) else {
            return ResponseTemplate::new(400);
        };
        let response_id = format!("resp-{sequence}-{prompt}");
        let response = ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(
                agent_sse(&response_id, answer, output_tokens),
                "text/event-stream",
            );
        if stalled {
            response.set_delay(STALLED_RESPONSE_DELAY)
        } else {
            response
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_cli_run_replays_only_its_completed_prefix_and_honors_fingerprint() -> Result<()>
{
    let server = MockServer::start().await;
    let responder = ControlledAgentResponder::default();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(responder.clone())
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let script_path = workspace.path().join("partial-resume.js");
    std::fs::write(&script_path, WORKFLOW_SOURCE)?;
    write_profile_config(codex_home.path(), &server, ORIGINAL_MODEL)?;

    let mut source_command = workflow_run_command(
        codex_home.path(),
        workspace.path(),
        /*resume_run_id*/ None,
    )?;
    source_command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut source_child = source_command.spawn()?;
    let source_run_dir =
        wait_for_durable_prefix(codex_home.path(), &responder, &mut source_child).await?;
    let source_run_id = run_id_from_dir(&source_run_dir)?;

    let source_calls_before_kill = agent_calls(&source_run_dir)?;
    assert_eq!(source_calls_before_kill.len(), 1, "k = 1 durable call");
    assert_eq!(source_calls_before_kill[0]["ordinal"], 0);
    assert_eq!(source_calls_before_kill[0]["status"], "completed");
    assert_eq!(source_calls_before_kill[0]["tokens_spent"], 17);
    assert_eq!(source_calls_before_kill[0]["return"], "answer:first");
    assert_eq!(request_prompts(&responder.snapshot()), ["first", "second"]);

    // Kill and reap only the explicitly spawned product fixture. No build or
    // test-runner process is signalled. The process-owned host observes its IPC
    // parent closing and exits independently.
    source_child.start_kill()?;
    let killed = timeout(DEFAULT_TIMEOUT, source_child.wait_with_output())
        .await
        .context("timed out reaping interrupted workflow CLI")??;
    assert!(!killed.status.success(), "the source CLI was interrupted");

    let source_lines = journal_lines(&source_run_dir)?;
    let source_calls = sorted_lines(&source_lines, "agent_call");
    assert_eq!(source_calls.len(), 1, "interruption must not invent a call");
    let source_first_child = required_string(&source_calls[0], "child_thread_id")?;
    let source_first_rollout = required_string(&source_calls[0], "rollout_path")?;
    let stalled_bound = sorted_lines(&source_lines, "agent_bound")
        .into_iter()
        .find(|line| line["ordinal"] == 1)
        .context("the stalled second child must be durably bound")?;
    let stalled_child = required_string(&stalled_bound, "child_thread_id")?;

    // The detached watcher owns crash recovery: it observes the released lease,
    // marks the former live run interrupted, and terminates deterministically.
    let source_frame = watch_json(codex_home.path(), workspace.path(), &source_run_id).await?;
    assert_eq!(source_frame["terminal"], true);
    assert_eq!(source_frame["status"], "failed");
    assert_eq!(source_frame["budget"]["total"], BUDGET_TOTAL);
    assert_eq!(agent_calls(&source_run_dir)?.len(), 1);
    assert_eq!(
        read_json(source_run_dir.join("meta.json"))?["status"],
        "failed"
    );
    assert_eq!(
        read_json(source_run_dir.join("progress.json"))?["state"],
        "terminal"
    );

    let requests_before_resume = responder.snapshot();
    let unchanged = run_success(
        workflow_run_command(codex_home.path(), workspace.path(), Some(&source_run_id))?,
        "unchanged partial-prefix resume",
    )
    .await?;
    let unchanged_stdout = String::from_utf8(unchanged.stdout)?;
    let resumed_run_id = run_id_from_stdout(&unchanged_stdout)?;
    assert_ne!(resumed_run_id, source_run_id, "resume mints a fresh run ID");
    let resumed_result = result_from_stdout(&unchanged_stdout)?;
    assert_eq!(resumed_result, expected_result());

    let requests_after_resume = responder.snapshot();
    assert_eq!(
        &requests_after_resume[requests_before_resume.len()..],
        &[
            ObservedRequest {
                prompt: "second".to_string(),
                model: ORIGINAL_MODEL.to_string(),
                stalled: false,
            },
            ObservedRequest {
                prompt: "third".to_string(),
                model: ORIGINAL_MODEL.to_string(),
                stalled: false,
            },
        ],
        "k = 1 cached and N-k = 2 live calls"
    );
    assert_eq!(
        requests_after_resume
            .iter()
            .filter(|request| request.prompt == "first")
            .count(),
        1,
        "the cached first call must never reach the provider twice"
    );

    let resumed_run_dir = runs_root(codex_home.path()).join(&resumed_run_id);
    let resumed_calls = agent_calls(&resumed_run_dir)?;
    assert_agent_call_spend(&resumed_calls);
    assert_eq!(
        required_string(&resumed_calls[0], "child_thread_id")?,
        source_first_child,
        "cached linkage is preserved"
    );
    assert_eq!(
        required_string(&resumed_calls[0], "rollout_path")?,
        source_first_rollout,
        "cached rollout linkage is preserved"
    );
    let resumed_second_child = required_string(&resumed_calls[1], "child_thread_id")?;
    let resumed_third_child = required_string(&resumed_calls[2], "child_thread_id")?;
    assert_ne!(
        resumed_second_child, stalled_child,
        "the interrupted live child is replaced by a fresh child"
    );
    assert_ne!(resumed_second_child, source_first_child);
    assert_ne!(resumed_third_child, source_first_child);
    assert_ne!(resumed_third_child, resumed_second_child);
    assert_terminal_run(
        codex_home.path(),
        workspace.path(),
        &resumed_run_id,
        &source_run_id,
    )
    .await?;

    let source_meta = read_json(source_run_dir.join("meta.json"))?;
    let resumed_meta = read_json(resumed_run_dir.join("meta.json"))?;
    assert_eq!(resumed_meta["script_hash"], source_meta["script_hash"]);
    assert_eq!(resumed_meta["args_hash"], source_meta["args_hash"]);
    assert_eq!(
        resumed_meta["execution_fingerprint"],
        source_meta["execution_fingerprint"]
    );

    // Source and args remain byte-for-byte unchanged. Altering only the
    // inherited model changes the execution fingerprint and forces all N calls
    // live, including ordinal zero.
    write_profile_config(codex_home.path(), &server, CHANGED_MODEL)?;
    let requests_before_changed = responder.snapshot();
    let changed = run_success(
        workflow_run_command(codex_home.path(), workspace.path(), Some(&source_run_id))?,
        "changed-fingerprint resume",
    )
    .await?;
    let changed_stdout = String::from_utf8(changed.stdout)?;
    let changed_run_id = run_id_from_stdout(&changed_stdout)?;
    assert_ne!(changed_run_id, source_run_id);
    assert_ne!(changed_run_id, resumed_run_id);
    assert_eq!(result_from_stdout(&changed_stdout)?, expected_result());

    let requests_after_changed = responder.snapshot();
    assert_eq!(
        &requests_after_changed[requests_before_changed.len()..],
        &[
            ObservedRequest {
                prompt: "first".to_string(),
                model: CHANGED_MODEL.to_string(),
                stalled: false,
            },
            ObservedRequest {
                prompt: "second".to_string(),
                model: CHANGED_MODEL.to_string(),
                stalled: false,
            },
            ObservedRequest {
                prompt: "third".to_string(),
                model: CHANGED_MODEL.to_string(),
                stalled: false,
            },
        ],
        "execution-fingerprint divergence must force N live calls"
    );

    let changed_run_dir = runs_root(codex_home.path()).join(&changed_run_id);
    let changed_calls = agent_calls(&changed_run_dir)?;
    assert_agent_call_spend(&changed_calls);
    assert_ne!(
        required_string(&changed_calls[0], "child_thread_id")?,
        source_first_child,
        "fingerprint divergence must not reuse ordinal-zero linkage"
    );
    let changed_meta = read_json(changed_run_dir.join("meta.json"))?;
    assert_ne!(
        changed_meta["execution_fingerprint"],
        source_meta["execution_fingerprint"]
    );
    assert_terminal_run(
        codex_home.path(),
        workspace.path(),
        &changed_run_id,
        &source_run_id,
    )
    .await?;

    Ok(())
}

async fn wait_for_durable_prefix(
    codex_home: &Path,
    responder: &ControlledAgentResponder,
    child: &mut Child,
) -> Result<PathBuf> {
    let deadline = Instant::now() + DEFAULT_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("source workflow exited before interruption point: {status}");
        }
        if let Some(run_dir) = only_run_dir(codex_home)?
            && responder
                .snapshot()
                .iter()
                .any(|request| request.prompt == "second" && request.stalled)
            && agent_calls(&run_dir).is_ok_and(|calls| {
                calls.len() == 1 && calls[0]["ordinal"] == 0 && calls[0]["status"] == "completed"
            })
        {
            return Ok(run_dir);
        }
        anyhow::ensure!(
            Instant::now() < deadline,
            "timed out waiting for one durable call and a stalled second agent"
        );
        sleep(Duration::from_millis(20)).await;
    }
}

fn workflow_run_command(
    codex_home: &Path,
    workspace: &Path,
    resume_run_id: Option<&str>,
) -> Result<Command> {
    let mut command = base_command(codex_home, workspace)?;
    command.args([
        "workflow",
        "run",
        "partial-resume.js",
        "--args",
        WORKFLOW_ARGS,
    ]);
    if let Some(run_id) = resume_run_id {
        command.args(["--resume", run_id]);
    }
    Ok(command)
}

fn base_command(codex_home: &Path, workspace: &Path) -> Result<Command> {
    let mut command = Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    command
        .env("CODEX_HOME", codex_home)
        // This only pins the sibling executable used by the default
        // process-owned host selection; it does not switch to the in-process host.
        .env(
            "CODEX_CODE_MODE_HOST_PATH",
            codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")?,
        )
        .args(["--profile", PROFILE, "-C", &workspace.to_string_lossy()])
        .kill_on_drop(true);
    Ok(command)
}

async fn watch_json(codex_home: &Path, workspace: &Path, run_id: &str) -> Result<Value> {
    let mut command = base_command(codex_home, workspace)?;
    command.args(["workflow", "watch", run_id, "--json"]);
    let output = run_success(command, "workflow watch --json").await?;
    let stdout = String::from_utf8(output.stdout)?;
    stdout
        .lines()
        .rev()
        .find_map(|line| serde_json::from_str(line).ok())
        .context("workflow watch did not emit a JSON frame")
}

async fn run_success(mut command: Command, description: &str) -> Result<std::process::Output> {
    let output = timeout(DEFAULT_TIMEOUT, command.output())
        .await
        .with_context(|| format!("timed out waiting for {description}"))??;
    anyhow::ensure!(
        output.status.success(),
        "{description} failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

async fn assert_terminal_run(
    codex_home: &Path,
    workspace: &Path,
    run_id: &str,
    source_run_id: &str,
) -> Result<()> {
    let run_dir = runs_root(codex_home).join(run_id);
    let meta = read_json(run_dir.join("meta.json"))?;
    assert_eq!(meta["run_id"], run_id);
    assert_eq!(meta["parent_run_id"], Value::Null);
    assert_eq!(meta["resumed_from_run_id"], source_run_id);
    assert_eq!(meta["status"], "completed");

    let progress = read_json(run_dir.join("progress.json"))?;
    assert_eq!(progress["run_id"], run_id);
    assert_eq!(progress["state"], "terminal");
    assert_eq!(progress["budget"]["spent"], 69);
    assert_eq!(progress["budget"]["total"], BUDGET_TOTAL);
    let topology = progress["topology"]
        .as_object()
        .context("progress topology")?;
    let agents = topology
        .values()
        .filter(|node| node["node_type"] == "agent")
        .collect::<Vec<_>>();
    assert_eq!(agents.len(), 3);
    assert!(agents.iter().all(|agent| agent["state"] == "completed"));

    let frame = watch_json(codex_home, workspace, run_id).await?;
    assert_eq!(frame["runId"], run_id);
    assert_eq!(frame["terminal"], true);
    assert_eq!(frame["status"], "completed");
    assert_eq!(frame["budget"]["spent"], 69);
    assert_eq!(frame["budget"]["total"], BUDGET_TOTAL);
    assert_eq!(
        frame["nodes"]
            .as_array()
            .context("watch nodes")?
            .iter()
            .filter(|node| node["kind"] == "agent" && node["status"] == "completed")
            .count(),
        3
    );
    Ok(())
}

fn write_profile_config(codex_home: &Path, server: &MockServer, model: &str) -> Result<()> {
    std::fs::write(
        codex_home.join(format!("{PROFILE}.config.toml")),
        format!(
            r#"
model = "{model}"
model_provider = "fixture_router"
model_reasoning_effort = "medium"
approval_policy = "never"
sandbox_mode = "read-only"

[features]
workflow = true

[model_providers.fixture_router]
name = "Partial resume fixture router"
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

fn expected_result() -> Value {
    json!({
        "results": ["answer:first", "answer:second", "answer:third"],
        "firstSpent": 17,
        "spent": 69,
        "total": BUDGET_TOTAL,
        "remaining": BUDGET_TOTAL - 69,
    })
}

fn assert_agent_call_spend(calls: &[Value]) {
    assert_eq!(calls.len(), 3);
    assert_eq!(
        calls
            .iter()
            .map(|call| {
                (
                    call["ordinal"].as_u64(),
                    call["status"].as_str(),
                    call["return"].as_str(),
                    call["tokens_spent"].as_u64(),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (Some(0), Some("completed"), Some("answer:first"), Some(17)),
            (Some(1), Some("completed"), Some("answer:second"), Some(23)),
            (Some(2), Some("completed"), Some("answer:third"), Some(29)),
        ]
    );
}

fn fixture_answer(prompt: &str) -> Option<(&'static str, i64)> {
    match prompt {
        "first" => Some(("answer:first", 17)),
        "second" => Some(("answer:second", 23)),
        "third" => Some(("answer:third", 29)),
        _ => None,
    }
}

fn last_user_text(body: &Value) -> Option<&str> {
    body.get("input")?
        .as_array()?
        .iter()
        .rev()
        .find(|item| item.get("role").and_then(Value::as_str) == Some("user"))?
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|content| content.get("text").and_then(Value::as_str))
}

fn agent_sse(response_id: &str, answer: &str, output_tokens: i64) -> String {
    let events = [
        json!({
            "type": "response.created",
            "response": { "id": response_id },
        }),
        json!({
            "type": "response.output_item.done",
            "item": {
                "type": "message",
                "role": "assistant",
                "id": format!("msg-{response_id}"),
                "content": [{ "type": "output_text", "text": answer }],
            },
        }),
        json!({
            "type": "response.completed",
            "response": {
                "id": response_id,
                "usage": {
                    "input_tokens": 10,
                    "input_tokens_details": null,
                    "output_tokens": output_tokens,
                    "output_tokens_details": null,
                    "total_tokens": 10 + output_tokens,
                },
            },
        }),
    ];
    events
        .into_iter()
        .map(|event| {
            let kind = event["type"].as_str().expect("event type");
            format!("event: {kind}\ndata: {event}\n\n")
        })
        .collect()
}

fn runs_root(codex_home: &Path) -> PathBuf {
    codex_home.join("workflows/runs")
}

fn only_run_dir(codex_home: &Path) -> Result<Option<PathBuf>> {
    let root = runs_root(codex_home);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let dirs = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect::<Vec<_>>();
    anyhow::ensure!(dirs.len() <= 1, "expected at most one source run: {dirs:?}");
    Ok(dirs.into_iter().next())
}

fn run_id_from_dir(run_dir: &Path) -> Result<String> {
    run_dir
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
        .context("workflow run directory has no UTF-8 run id")
}

fn run_id_from_stdout(stdout: &str) -> Result<String> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run ID: "))
        .map(str::to_string)
        .context("workflow CLI did not print a run ID")
}

fn result_from_stdout(stdout: &str) -> Result<Value> {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|value| value.get("results").is_some())
        .context("workflow CLI did not print its JSON result")
}

fn request_prompts(requests: &[ObservedRequest]) -> Vec<&str> {
    requests
        .iter()
        .map(|request| request.prompt.as_str())
        .collect()
}

fn read_json(path: impl AsRef<Path>) -> Result<Value> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn journal_lines(run_dir: &Path) -> Result<Vec<Value>> {
    std::fs::read_to_string(run_dir.join("journal.jsonl"))?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn sorted_lines(lines: &[Value], kind: &str) -> Vec<Value> {
    let mut selected = lines
        .iter()
        .filter(|line| line["type"] == kind)
        .cloned()
        .collect::<Vec<_>>();
    selected.sort_by_key(|line| line["ordinal"].as_u64());
    selected
}

fn agent_calls(run_dir: &Path) -> Result<Vec<Value>> {
    Ok(sorted_lines(&journal_lines(run_dir)?, "agent_call"))
}

fn required_string(value: &Value, field: &str) -> Result<String> {
    value[field]
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .with_context(|| format!("journal field `{field}` must be a non-empty string"))
}

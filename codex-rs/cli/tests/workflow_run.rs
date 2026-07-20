//! End-to-end coverage for the `codex workflow run` entrypoint (`P4-cli-run`).
//!
//! This is the real-entrypoint proof for GitHub #28: the M0-M3 Dynamic Workflows
//! engine was fully built and unit/integration-tested via
//! `run_workflow_source`, but there was NO way to invoke it — no CLI command and
//! no model-callable workflow tool. These tests drive the assembled `codex`
//! binary end-to-end on a `phase()`/`log()`-only workflow and assert it runs with
//! the workflow narrator globals installed (`phase`/`log` are DEFINED) and with
//! ZERO model calls (no auth/model is configured, and the run still succeeds).

#![allow(clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use predicates::str::contains;
use serde_json::Value as JsonValue;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn codex_command(codex_home: &Path) -> Result<assert_cmd::Command> {
    let mut cmd = assert_cmd::Command::new(codex_utils_cargo_bin::cargo_bin("codex")?);
    cmd.env("CODEX_HOME", codex_home);
    Ok(cmd)
}

fn run_id_from_stdout(stdout: &str) -> Option<&str> {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix("run ID: "))
}

fn journal_agent_lines(run_dir: &Path) -> Result<Vec<JsonValue>> {
    let journal = std::fs::read_to_string(run_dir.join("journal.jsonl"))?;
    let mut lines = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<JsonValue>(line).ok())
        .filter(|line| line["type"] == "agent_call")
        .collect::<Vec<_>>();
    lines.sort_by_key(|line| line["ordinal"].as_u64());
    Ok(lines)
}

#[derive(Clone, Default)]
struct AgentResponder {
    requests: Arc<Mutex<Vec<JsonValue>>>,
}

impl Respond for AgentResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: JsonValue =
            serde_json::from_slice(&request.body).expect("responses request JSON");
        self.requests
            .lock()
            .expect("requests lock")
            .push(body.clone());
        let prompt = last_user_text(&body).unwrap_or_default();
        let (response_id, answer) = if prompt == "first" {
            ("resp-first", "answer:first")
        } else if prompt == "second" {
            ("resp-second", "answer:second")
        } else {
            ("resp-unexpected", "answer:unexpected")
        };
        ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_raw(agent_sse(response_id, answer), "text/event-stream")
    }
}

fn last_user_text(body: &JsonValue) -> Option<&str> {
    body.get("input")?
        .as_array()?
        .iter()
        .rev()
        .find(|item| item.get("role").and_then(JsonValue::as_str) == Some("user"))?
        .get("content")?
        .as_array()?
        .iter()
        .find_map(|content| content.get("text").and_then(JsonValue::as_str))
}

fn agent_sse(response_id: &str, answer: &str) -> String {
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
                    "output_tokens": 3,
                    "output_tokens_details": null,
                    "total_tokens": 13,
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

fn write_profile_config(codex_home: &Path, profile: &str, server: &MockServer) -> Result<()> {
    std::fs::write(
        codex_home.join(format!("{profile}.config.toml")),
        format!(
            r#"
model = "fixture-model"
model_provider = "fixture_router"
model_reasoning_effort = "medium"
approval_policy = "never"
sandbox_mode = "read-only"

[features]
workflow = true

[model_providers.fixture_router]
name = "Workflow fixture router"
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

/// A `phase()`/`log()`-only workflow runs end-to-end through `codex workflow run`
/// with the workflow globals installed and no model call, printing its top-level
/// `text(...)` return value to stdout and exiting 0.
#[test]
fn workflow_run_executes_phase_and_log_workflow_end_to_end() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;

    // A workflow whose orchestration is entirely `phase()`/`log()`/`text()`:
    // reaching the trailing `text(...)` proves the workflow narrator globals were
    // installed for this run (they would be `undefined` under the plain code-mode
    // `exec` tool, which runs with `workflow: false`).
    let script = concat!(
        "export const meta = { name: 'cli-demo', description: 'demo', phases: ['plan'] };\n",
        "phase('plan');\n",
        "log('starting the run');\n",
        "text('workflow-ran');\n",
    );
    let script_path = workspace.path().join("cli-demo.js");
    std::fs::write(&script_path, script)?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        &script_path.to_string_lossy(),
    ])
    .assert()
    .success()
    .stdout(contains("workflow-ran"));

    Ok(())
}

#[test]
fn workflow_watch_renders_a_completed_durable_run_end_to_end() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let script_path = workspace.path().join("watch-demo.js");
    std::fs::write(
        &script_path,
        concat!(
            "export const meta = { name: 'watch-demo', description: 'demo', phases: ['verify'] };\n",
            "phase('verify');\n",
            "log('durable watch fixture');\n",
            "text('done');\n",
        ),
    )?;

    let mut run = codex_command(codex_home.path())?;
    let run_output = run
        .args([
            "--enable",
            "workflow",
            "workflow",
            "run",
            &script_path.to_string_lossy(),
        ])
        .output()?;
    assert!(run_output.status.success());
    let run_stdout = String::from_utf8(run_output.stdout)?;
    let run_id = run_id_from_stdout(&run_stdout).expect("run id");

    let mut watch = codex_command(codex_home.path())?;
    watch
        .args(["--enable", "workflow", "workflow", "watch", run_id])
        .assert()
        .success()
        .stdout(contains("workflow watch-demo"))
        .stdout(contains("[completed]"))
        .stdout(contains("phase 1: verify [completed]"));

    let mut json_watch = codex_command(codex_home.path())?;
    let json_output = json_watch
        .args([
            "--enable", "workflow", "workflow", "watch", run_id, "--json",
        ])
        .output()?;
    assert!(json_output.status.success());
    let json_stdout = String::from_utf8(json_output.stdout)?;
    let lines = json_stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 1, "completed watch emits one NDJSON frame");
    let frame: JsonValue = serde_json::from_str(lines[0])?;
    assert_eq!(frame["runId"], run_id);
    assert_eq!(frame["name"], "watch-demo");
    assert_eq!(frame["status"], "completed");
    assert_eq!(frame["terminal"], true);
    assert_eq!(frame["phases"][0]["status"], "completed");

    Ok(())
}

/// `--args <json>` is injected read-only as the workflow `args` global and
/// reaches the isolate; invalid JSON is rejected up front.
#[test]
fn workflow_run_threads_args_into_the_isolate() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;

    let script = concat!(
        "export const meta = { name: 'args-demo', description: 'demo' };\n",
        "text(String(args.who));\n",
    );
    let script_path = workspace.path().join("args-demo.js");
    std::fs::write(&script_path, script)?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        &script_path.to_string_lossy(),
        "--args",
        r#"{"who":"from-cli"}"#,
    ])
    .assert()
    .success()
    .stdout(contains("from-cli"));

    // Invalid JSON is rejected before any isolate execution.
    let mut bad = codex_command(codex_home.path())?;
    bad.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        &script_path.to_string_lossy(),
        "--args",
        "not-json",
    ])
    .assert()
    .failure()
    .stderr(contains("invalid --args JSON"));

    Ok(())
}

/// The subcommand is gated behind `Feature::Workflow`: without `--enable
/// workflow` the run is rejected rather than silently running through the plain
/// code-mode path.
#[test]
fn workflow_run_requires_the_workflow_feature() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;

    let script = concat!(
        "export const meta = { name: 'gated', description: 'demo' };\n",
        "text('should-not-run');\n",
    );
    let script_path = workspace.path().join("gated.js");
    std::fs::write(&script_path, script)?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args(["workflow", "run", &script_path.to_string_lossy()])
        .assert()
        .failure()
        .stderr(contains("workflow"));

    Ok(())
}

/// A missing target is rejected with a clear error (neither an existing path nor
/// a saved workflow name).
#[test]
fn workflow_run_rejects_unknown_target() -> Result<()> {
    let codex_home = TempDir::new()?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        "no-such-workflow",
    ])
    .assert()
    .failure()
    .stderr(contains("did not resolve"));

    Ok(())
}

/// The assembled binary uses its default process-owned host, a real registered Session/Turn, and
/// the selected profile's provider for child agents. The relative target is resolved from `-C`,
/// both children make fixture model calls with inherited/overridden model and effort, and their
/// durable journal and progress files carry terminal child linkage.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_run_default_process_host_fans_out_through_profile_provider() -> Result<()> {
    let server = MockServer::start().await;
    let responder = AgentResponder::default();
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(responder.clone())
        .expect(2)
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    write_profile_config(codex_home.path(), "fanout", &server)?;
    std::fs::write(
        workspace.path().join("fanout.js"),
        concat!(
            "export const meta = { name: 'fanout', description: 'fixture', phases: ['fanout'] };\n",
            "phase('fanout');\n",
            "log('starting agents');\n",
            "const values = await parallel([() => agent('first'), () => agent('second', { model: 'gpt-5.5', effort: 'high' })]);\n",
            "text(JSON.stringify(values));\n",
        ),
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--profile",
        "fanout",
        "-C",
        &workspace.path().to_string_lossy(),
        "workflow",
        "run",
        "fanout.js",
    ]);
    let output = cmd.output()?;
    assert!(
        output.status.success(),
        "workflow CLI failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout)?;
    assert!(stdout.contains("phase: fanout"), "stdout: {stdout}");
    assert!(stdout.contains("log: starting agents"), "stdout: {stdout}");
    assert!(
        stdout.contains(r#"["answer:first","answer:second"]"#),
        "stdout: {stdout}"
    );
    let first_run_id = run_id_from_stdout(&stdout)
        .expect("successful run prints its run ID")
        .to_string();

    let requests = responder.requests.lock().expect("requests lock").clone();
    assert_eq!(requests.len(), 2, "one model call per child agent");
    let mut request_configs = requests
        .iter()
        .map(|request| {
            (
                last_user_text(request).expect("child prompt"),
                request["model"].as_str().expect("child model"),
                request
                    .pointer("/reasoning/effort")
                    .and_then(JsonValue::as_str)
                    .expect("child effort"),
            )
        })
        .collect::<Vec<_>>();
    request_configs.sort_unstable();
    assert_eq!(
        request_configs,
        vec![
            ("first", "fixture-model", "medium"),
            ("second", "gpt-5.5", "high"),
        ]
    );
    let mut prompts = requests
        .iter()
        .filter_map(last_user_text)
        .map(str::to_string)
        .collect::<Vec<_>>();
    prompts.sort();
    assert_eq!(prompts, vec!["first".to_string(), "second".to_string()]);

    let runs_root = codex_home.path().join("workflows/runs");
    let first_run_dir = runs_root.join(&first_run_id);
    let agent_lines = journal_agent_lines(&first_run_dir)?;
    assert_eq!(agent_lines.len(), 2);
    for line in &agent_lines {
        assert_eq!(line["status"], "completed");
        assert!(
            line["child_thread_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(
            line["rollout_path"]
                .as_str()
                .is_some_and(|path| !path.is_empty())
        );
    }
    let progress: JsonValue =
        serde_json::from_slice(&std::fs::read(first_run_dir.join("progress.json"))?)?;
    assert_eq!(progress["state"], "terminal");
    assert_eq!(progress["status"], json!({"completed": null}));
    assert_eq!(
        progress["topology"]
            .as_object()
            .expect("progress topology")
            .values()
            .filter(|node| node["node_type"] == "agent")
            .map(|node| (node["state"].clone(), node["status"].clone()))
            .collect::<Vec<_>>(),
        vec![
            (json!("completed"), json!({"completed": null})),
            (json!("completed"), json!({"completed": null})),
        ]
    );
    let original_linkage = agent_lines
        .iter()
        .map(|line| {
            (
                line["ordinal"].clone(),
                line["child_thread_id"].clone(),
                line["rollout_path"].clone(),
                line["return"].clone(),
            )
        })
        .collect::<Vec<_>>();

    // Resume the unchanged source through a second assembled CLI process. The
    // cached prefix must produce the same result and linkage without another
    // request to the fixture provider.
    let mut resume_cmd = codex_command(codex_home.path())?;
    resume_cmd.args([
        "--profile",
        "fanout",
        "-C",
        &workspace.path().to_string_lossy(),
        "workflow",
        "run",
        "fanout.js",
        "--resume",
        &first_run_id,
    ]);
    let resumed = resume_cmd.output()?;
    assert!(
        resumed.status.success(),
        "resume failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&resumed.stdout),
        String::from_utf8_lossy(&resumed.stderr),
    );
    let resumed_stdout = String::from_utf8(resumed.stdout)?;
    assert!(
        resumed_stdout.contains(r#"["answer:first","answer:second"]"#),
        "stdout: {resumed_stdout}"
    );
    let resumed_run_id = run_id_from_stdout(&resumed_stdout)
        .expect("resumed run prints its fresh run ID")
        .to_string();
    assert_ne!(resumed_run_id, first_run_id);
    assert_eq!(
        responder.requests.lock().expect("requests lock").len(),
        2,
        "unchanged resume must make zero new model requests"
    );

    let replayed_lines = journal_agent_lines(&runs_root.join(&resumed_run_id))?;
    let replayed_linkage = replayed_lines
        .iter()
        .map(|line| {
            (
                line["ordinal"].clone(),
                line["child_thread_id"].clone(),
                line["rollout_path"].clone(),
                line["return"].clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(replayed_linkage, original_linkage);
    let resumed_meta: JsonValue = serde_json::from_slice(&std::fs::read(
        runs_root.join(resumed_run_id).join("meta.json"),
    )?)?;
    assert_eq!(resumed_meta["parent_run_id"], JsonValue::Null);
    assert_eq!(resumed_meta["resumed_from_run_id"], first_run_id);

    Ok(())
}

#[test]
fn workflow_run_rejects_oversized_source_before_execution() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let script_path = workspace.path().join("too-large.js");
    let mut source = concat!(
        "export const meta = { name: 'large', description: 'large' };\n",
        "text('must-not-run');\n",
    )
    .to_string();
    source.push_str(&" ".repeat(1024 * 1024));
    std::fs::write(&script_path, source)?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        &script_path.to_string_lossy(),
    ])
    .assert()
    .failure()
    .stderr(contains("exceeds"));
    Ok(())
}

#[test]
fn workflow_run_rejects_oversized_args_before_parsing() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let script_path = workspace.path().join("args-cap.js");
    std::fs::write(
        &script_path,
        "export const meta = { name: 'args-cap', description: 'args cap' };\ntext('x');\n",
    )?;
    let args = format!(r#"{{"blob":"{}"}}"#, "x".repeat(65 * 1024));

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        &script_path.to_string_lossy(),
        "--args",
        &args,
    ])
    .assert()
    .failure()
    .stderr(contains("--args JSON exceeds"));
    Ok(())
}

#[test]
fn workflow_run_rejects_noncanonical_resume_id_before_path_lookup() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let script_path = workspace.path().join("resume.js");
    std::fs::write(
        &script_path,
        "export const meta = { name: 'resume', description: 'resume' };\ntext('x');\n",
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        &script_path.to_string_lossy(),
        "--resume",
        "../../escape",
    ])
    .assert()
    .failure()
    .stderr(contains("invalid --resume run id"));
    Ok(())
}

#[test]
fn workflow_run_surfaces_script_errors_with_nonzero_exit() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let script_path = workspace.path().join("throws.js");
    std::fs::write(
        &script_path,
        concat!(
            "export const meta = { name: 'throws', description: 'throws' };\n",
            "throw new Error('fixture-boom');\n",
        ),
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "workflow",
        "run",
        &script_path.to_string_lossy(),
    ])
    .assert()
    .failure()
    .stderr(contains("fixture-boom"));
    Ok(())
}

#[test]
fn workflow_run_resolves_saved_name_from_configured_cwd() -> Result<()> {
    let codex_home = TempDir::new()?;
    let workspace = TempDir::new()?;
    let saved_root = workspace.path().join(".codex/workflows");
    std::fs::create_dir_all(&saved_root)?;
    std::fs::write(
        saved_root.join("saved.js"),
        concat!(
            "export const meta = { name: 'saved-demo', description: 'saved' };\n",
            "text('saved-ran');\n",
        ),
    )?;

    let mut cmd = codex_command(codex_home.path())?;
    cmd.args([
        "--enable",
        "workflow",
        "-C",
        &workspace.path().to_string_lossy(),
        "workflow",
        "run",
        "saved-demo",
    ])
    .assert()
    .success()
    .stdout(contains("saved-ran"));
    Ok(())
}

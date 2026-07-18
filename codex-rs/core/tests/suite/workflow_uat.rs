#![allow(clippy::unwrap_used, clippy::expect_used)]
//! Phase-1 hermetic fixture-lane UAT gates for Dynamic Workflows (`P1-uat-gates`,
//! spec §14.2 UAT-3 / UAT-4, §14.3 Phase-1 exit gates).
//!
//! These drive a *real* Codex session end-to-end with `Feature::Workflow` enabled: the fixture
//! model emits a `workflow` custom-tool call whose body fans out real subagents via `parallel()` /
//! `agent()`. Every subagent is a real registered thread that issues its own model request to the
//! same ordered SSE fixture server, so the tests assert on ENGINE ARTIFACTS (the workflow's own
//! position-preserving return, per-child model/effort request bodies, per-subagent rollout files,
//! parent linkage in each subagent's `SessionMeta`) rather than model free text.
//!
//! ## Host path
//!
//! The fixture lane runs the code-mode isolate **in-process** (the `CoreTurnHost` spawn bridge),
//! not the out-of-process `codex-code-mode-host` binary. `Feature::CodeModeHost` is `default_enabled`
//! but is NOT a `Feature::Workflow` dependency, so each test explicitly disables it; the
//! `TestCodexBuilder` then selects the `InProcessCodeModeSessionProvider`. This exercises the same
//! in-process `agent()` -> `spawn_and_await_final_message` path proven at the unit level in
//! `core/src/agent/control/spawn_await_tests.rs`, now driven through the real V8 isolate and the
//! `parallel()` prelude.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_protocol::ThreadId;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_reasoning_item;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

/// Prefix stamped into every subagent prompt so the routing responder can recognise a subagent
/// turn (the prompt arrives as the subagent's own `user` message) and recover its ordinal.
const SUBAGENT_MARKER: &str = "WFUAT_SUB_";

/// Call id the fixture model uses for its `workflow` custom-tool call.
const WORKFLOW_CALL_ID: &str = "call-workflow";

/// Distinctive model slug a registered `reviewer` role locks onto the child config, so a test can
/// prove `opts.agentType` resolved and applied the role by observing the child's model request.
const REVIEWER_ROLE_MODEL: &str = "gpt-5.4-mini";

/// Parent (and thus inherited) model for these workflows. Distinct from the per-child overrides so
/// an inherited child config is distinguishable from an overridden one.
const PARENT_MODEL: &str = "gpt-5.5";

/// Decode a recorded request body (handles the zstd content-encoding the client may apply) into a
/// JSON value.
fn request_body_json(request: &wiremock::Request) -> Value {
    let is_zstd = request
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });
    let bytes = if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(request.body.as_slice()))
            .expect("decode zstd request body")
    } else {
        request.body.clone()
    };
    serde_json::from_slice(&bytes).expect("request body is JSON")
}

/// Extract the `user`-role message texts from a Responses API request `input`.
fn user_message_texts(body: &Value) -> Vec<String> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("type").and_then(Value::as_str) == Some("message")
                && item.get("role").and_then(Value::as_str) == Some("user")
        })
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|span| span.get("text").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// Recover the subagent ordinal carried by a request's `user` message marker, if any.
fn subagent_ordinal(body: &Value) -> Option<u64> {
    user_message_texts(body).iter().find_map(|text| {
        text.split(SUBAGENT_MARKER)
            .nth(1)
            .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
            .filter(|digits| !digits.is_empty())
            .and_then(|digits| digits.parse::<u64>().ok())
    })
}

/// True when the request `input` already carries a tool-call output for the workflow tool — i.e.
/// the fixture model has already run the workflow body and is being called again to finish the
/// parent turn.
fn contains_workflow_tool_output(body: &Value) -> bool {
    body["input"].as_array().into_iter().flatten().any(|item| {
        matches!(
            item.get("type").and_then(Value::as_str),
            Some("custom_tool_call_output") | Some("function_call_output")
        ) && item.get("call_id").and_then(Value::as_str) == Some(WORKFLOW_CALL_ID)
    })
}

/// True when the request `input` already carries any `function_call_output` (used to distinguish a
/// subagent's second model round-trip, after its first-turn tool call, from its first).
fn contains_any_function_call_output(body: &Value) -> bool {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call_output"))
}

/// One scripted subagent turn. Given the ordinal and whether this is the subagent's follow-up
/// (post tool-call) round-trip, returns the SSE body to serve.
type SubagentScript = Box<dyn Fn(u64, bool) -> String + Send + Sync>;

/// A single wiremock `Respond` that routes every `/responses` request to the right scripted SSE by
/// inspecting the request body, so the test is immune to the nondeterministic order in which
/// concurrent subagents hit the server:
/// - a subagent turn (its `user` message carries `WFUAT_SUB_<k>`) -> the per-ordinal script;
/// - the parent's post-workflow turn (input carries the workflow tool output) -> a final message;
/// - otherwise the parent's opening turn -> the `workflow` custom-tool call carrying the body.
struct WorkflowRouter {
    workflow_source: String,
    subagent: SubagentScript,
    /// Every decoded request body, in arrival order, for post-hoc assertions.
    seen: Arc<Mutex<Vec<Value>>>,
}

impl Respond for WorkflowRouter {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = request_body_json(request);
        self.seen.lock().unwrap().push(body.clone());

        // Subagent turn: its own `user` message is the prompt carrying the marker.
        if let Some(ordinal) = subagent_ordinal(&body) {
            let is_followup = contains_any_function_call_output(&body);
            return sse_response((self.subagent)(ordinal, is_followup));
        }

        // Parent's closing turn: the workflow already ran and its output is in history.
        if contains_workflow_tool_output(&body) {
            return sse_response(sse(vec![
                ev_response_created("resp-parent-final"),
                ev_assistant_message("msg-parent-final", "workflow done"),
                ev_completed("resp-parent-final"),
            ]));
        }

        // Parent's opening turn: emit the workflow custom-tool call.
        sse_response(sse(vec![
            ev_response_created("resp-parent-open"),
            responses::ev_custom_tool_call(WORKFLOW_CALL_ID, "workflow", &self.workflow_source),
            ev_completed("resp-parent-open"),
        ]))
    }
}

/// Mount the routing responder on the mock server and return the shared request log.
async fn mount_workflow_router(
    server: &MockServer,
    workflow_source: &str,
    subagent: SubagentScript,
) -> Arc<Mutex<Vec<Value>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = WorkflowRouter {
        workflow_source: workflow_source.to_string(),
        subagent,
        seen: Arc::clone(&seen),
    };
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(router)
        .mount(server)
        .await;
    seen
}

/// Persist the mock model provider (pointing at `server_uri`) into the on-disk `config.toml` so any
/// config **reload** — e.g. the role-layer reload that `opts.agentType` triggers via
/// `apply_role_to_config` -> `build_next_config` — reconstructs the provider from disk with the mock
/// `base_url` intact. Without this the reload would rebuild the built-in `openai` provider from disk
/// (defaulting `base_url` to the real API), and a role-child's request would leave the hermetic
/// lane and die. Mirrors `app-server/tests/common/config.rs::write_mock_responses_config_toml`.
fn write_mock_provider_config(codex_home: &std::path::Path, server_uri: &str) {
    let config_toml = codex_home.join("config.toml");
    std::fs::write(
        &config_toml,
        format!("openai_base_url = \"{server_uri}/v1\"\n"),
    )
    .expect("write mock provider config.toml");
}

/// Build a `TestCodexBuilder` with `Feature::Workflow` enabled and the process-host disabled so the
/// in-process code-mode isolate + spawn bridge is exercised. Also registers a `reviewer` role whose
/// locked model (`REVIEWER_ROLE_MODEL`) is observable when `opts.agentType = "reviewer"` is applied,
/// and persists the mock provider to disk so the role-layer reload stays on the fixture server.
fn workflow_builder(server_uri: &str) -> TestCodexBuilder {
    let server_uri = server_uri.to_string();
    test_codex()
        .with_model(PARENT_MODEL)
        .with_pre_build_hook(move |home| write_mock_provider_config(home, &server_uri))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            // Run the code-mode isolate in-process (no external host binary) for the hermetic lane.
            config
                .features
                .disable(Feature::CodeModeHost)
                .expect("disable code-mode host feature");

            // Register a user-defined `reviewer` role that locks a distinctive model, mirroring the
            // role wiring in `agent/role_tests.rs`: a `reviewer.toml` on disk referenced from
            // `config.agent_roles`.
            let role_dir = config.codex_home.as_path().to_path_buf();
            std::fs::create_dir_all(&role_dir).expect("create codex home dir");
            let role_path = role_dir.join("reviewer.toml");
            std::fs::write(&role_path, format!("model = \"{REVIEWER_ROLE_MODEL}\"\n"))
                .expect("write reviewer role config");
            config.agent_roles.insert(
                "reviewer".to_string(),
                AgentRoleConfig {
                    description: Some("Review carefully.".to_string()),
                    config_file: Some(role_path),
                    nickname_candidates: None,
                },
            );
        })
}

/// Recover the workflow body's own return value (`text(JSON.stringify(results))`) from the parent's
/// closing turn: the workflow tool output is echoed into that request's `input`.
fn workflow_return_array(seen: &Arc<Mutex<Vec<Value>>>) -> Vec<Value> {
    let bodies = seen.lock().unwrap();
    for body in bodies.iter() {
        let Some(items) = body["input"].as_array() else {
            continue;
        };
        for item in items {
            if item.get("type").and_then(Value::as_str) != Some("custom_tool_call_output") {
                continue;
            }
            if item.get("call_id").and_then(Value::as_str) != Some(WORKFLOW_CALL_ID) {
                continue;
            }
            let text = workflow_output_text(item);
            // The workflow tool output carries an adapter status header line plus the script's
            // `text(...)` payloads; the JSON array is the last parseable line.
            if let Some(array) = text
                .lines()
                .rev()
                .find_map(|line| serde_json::from_str::<Vec<Value>>(line.trim()).ok())
            {
                return array;
            }
        }
    }
    panic!("workflow tool output with a JSON array return was never sent to the model");
}

/// Pull the plain-text body out of a `custom_tool_call_output` item (string or content-item forms).
fn workflow_output_text(item: &Value) -> String {
    match item.get("output") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(spans)) => spans
            .iter()
            .filter_map(|span| span.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Find the first captured subagent request body for the given ordinal (the initial round-trip: no
/// `function_call_output` present yet).
fn subagent_request_for(seen: &Arc<Mutex<Vec<Value>>>, ordinal: u64) -> Value {
    let bodies = seen.lock().unwrap();
    bodies
        .iter()
        .find(|body| subagent_ordinal(body) == Some(ordinal))
        .unwrap_or_else(|| panic!("no subagent request captured for ordinal {ordinal}"))
        .clone()
}

/// The `model` the subagent's request carried (the applied child config's model).
fn request_model(body: &Value) -> String {
    body["model"]
        .as_str()
        .expect("request carries a model")
        .to_string()
}

/// The `reasoning.effort` the subagent's request carried, if any.
fn request_effort(body: &Value) -> Option<String> {
    body.pointer("/reasoning/effort")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Structured-output schema used by the UAT-4 fan-out.
fn answer_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "answer": { "type": "string" } },
        "required": ["answer"],
        "additionalProperties": false,
    })
}

/// UAT-4 — fan-out + structured output + `opts` overrides.
///
/// A `parallel()` fan-out of N ordered StructuredOutput fixtures (one scripted to die in the middle
/// position) returns a position-preserving array with the dead agent -> `null`; the conformant
/// results come back as JS objects (proving `opts.schema`'s `jsonschema` recheck passed); and each
/// surviving child's model request reflects the requested `opts.model` / `opts.effort` /
/// `opts.agentType`, asserted via the spawn-config path (`build_agent_spawn_config` +
/// `SpawnAgentConfigOverrides::apply`) as it lands on the child's actual model request.
///
/// The fan-out width (N = 3) is sized to the default concurrency ceiling
/// (`effective_agent_max_threads` = `max_concurrent_threads_per_session - 1` = 3): a workflow's
/// completed subagents deliberately stay registered (for monitor / live-attach), so each occupies a
/// registry slot for the run's lifetime and the run is bounded by that ceiling. A wider fan-out is a
/// configuration concern (raise `max_concurrent_threads_per_session`), not a coverage gap for this
/// gate; the over-cap -> `null` degradation itself is exercised by the `agent_dispatch.rs`
/// `Rejected` case and the scheduler unit tests.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat4_parallel_fanout_structured_output_and_opts_overrides() -> Result<()> {
    let server = responses::start_mock_server().await;

    // Three ordered StructuredOutput fixtures: model+effort override, a dead agent (middle
    // position), and an agentType role override. Distinct per-child config lands on each child's own
    // model request; the dead agent holds its position as `null`.
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat4', description: 'fan-out + structured output' }};
const schema = {schema};
const results = await parallel([
  () => agent("{marker}0 return structured output", {{ schema, model: "gpt-5.4", effort: "high" }}),
  () => agent("{marker}1 return structured output", {{ schema }}),
  () => agent("{marker}2 return structured output", {{ schema, agentType: "reviewer" }}),
]);
text(JSON.stringify(results));
"#,
        schema = answer_schema(),
        marker = SUBAGENT_MARKER,
    );

    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        if ordinal == 1 {
            // Dead agent: a completed turn with no assistant message -> no final message -> null.
            return sse(vec![
                ev_response_created(&format!("resp-sub-{ordinal}")),
                ev_completed(&format!("resp-sub-{ordinal}")),
            ]);
        }
        // Conformant StructuredOutput: the schema recheck accepts it and it returns as a JS object.
        let structured = json!({ "answer": format!("child-{ordinal}") }).to_string();
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_assistant_message(&format!("msg-sub-{ordinal}"), &structured),
            ev_completed_with_tokens(&format!("resp-sub-{ordinal}"), 100 + ordinal as i64),
        ])
    });

    let seen = mount_workflow_router(&server, &workflow_source, subagent).await;

    let test = workflow_builder(&server.uri()).build(&server).await?;
    test.submit_turn("run the uat4 fan-out workflow").await?;

    // Position-preserving results with the dead agent -> null, and conformant results as objects.
    let results = workflow_return_array(&seen);
    assert_eq!(
        results.len(),
        3,
        "N structured results, position-preserving"
    );
    assert_eq!(
        results[0],
        json!({ "answer": "child-0" }),
        "opts.schema object passes the jsonschema recheck and returns as an object"
    );
    assert_eq!(
        results[1],
        Value::Null,
        "the dead agent resolves to null and holds its (middle) position"
    );
    assert_eq!(results[2], json!({ "answer": "child-2" }));

    // Per-child config reflects the requested opts, observed on each child's own model request.
    let child0 = subagent_request_for(&seen, 0);
    assert_eq!(request_model(&child0), "gpt-5.4", "opts.model applied");
    assert_eq!(
        request_effort(&child0).as_deref(),
        Some("high"),
        "opts.effort applied"
    );

    let child2 = subagent_request_for(&seen, 2);
    assert_eq!(
        request_model(&child2),
        REVIEWER_ROLE_MODEL,
        "opts.agentType resolved the reviewer role and applied its locked model"
    );

    Ok(())
}

/// A persisted `response_item` of the given `type` (and optional assistant role) present in the
/// rollout lines.
fn rollout_has_response_item(lines: &[Value], item_type: &str, assistant_only: bool) -> bool {
    lines.iter().any(|line| {
        if line.get("type").and_then(Value::as_str) != Some("response_item") {
            return false;
        }
        let payload = &line["payload"];
        if payload.get("type").and_then(Value::as_str) != Some(item_type) {
            return false;
        }
        !assistant_only || payload.get("role").and_then(Value::as_str) == Some("assistant")
    })
}

/// The `SessionMeta` recorded at the head of a rollout file (id + parent linkage). `SessionMetaLine`
/// flattens `SessionMeta`, so its fields live directly on the line `payload`.
fn rollout_session_meta(lines: &[Value]) -> Value {
    lines
        .iter()
        .find(|line| line.get("type").and_then(Value::as_str) == Some("session_meta"))
        .map(|line| line["payload"].clone())
        .expect("rollout should open with a session_meta line")
}

/// UAT-3 — per-agent session saved & recoverable.
///
/// A `parallel()` fan-out of three subagents runs to completion; each subagent is a real registered
/// thread that persists its own `rollout-<date>-<thread_id>.jsonl`. The test asserts exactly one
/// rollout file per subagent, each carrying the expected final `Reasoning` / `FunctionCall` /
/// `AgentMessage` items (per `rollout/src/policy.rs`), and that the parent linkage
/// (`SessionMeta.parent_thread_id` == the workflow thread) is present in every child rollout.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat3_per_subagent_sessions_saved_and_recoverable() -> Result<()> {
    const N: u64 = 3;
    let server = responses::start_mock_server().await;

    let mut thunks = String::new();
    for k in 0..N {
        thunks.push_str(&format!(
            "  () => agent(\"{SUBAGENT_MARKER}{k} do the work\"),\n"
        ));
    }
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat3', description: 'per-agent session save' }};
const results = await parallel([
{thunks}]);
text(JSON.stringify(results));
"#
    );

    // Each subagent runs a two-round-trip turn: (1) reasoning + a `update_plan` function call, then
    // (2) its final assistant message. This persists a Reasoning, a FunctionCall, and an
    // AgentMessage item per subagent rollout (per the rollout persistence policy).
    let subagent: SubagentScript = Box::new(|ordinal, is_followup| {
        if is_followup {
            return sse(vec![
                ev_response_created(&format!("resp-sub-{ordinal}-b")),
                ev_assistant_message(&format!("msg-sub-{ordinal}"), &format!("done-{ordinal}")),
                ev_completed(&format!("resp-sub-{ordinal}-b")),
            ]);
        }
        let plan_args = json!({
            "plan": [{ "step": format!("step-{ordinal}"), "status": "in_progress" }],
        })
        .to_string();
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}-a")),
            ev_reasoning_item(
                &format!("reason-sub-{ordinal}"),
                &[&format!("thinking about {ordinal}")],
                &[],
            ),
            ev_function_call(&format!("call-plan-{ordinal}"), "update_plan", &plan_args),
            ev_completed(&format!("resp-sub-{ordinal}-a")),
        ])
    });

    let _seen = mount_workflow_router(&server, &workflow_source, subagent).await;

    let test = workflow_builder(&server.uri()).build(&server).await?;
    let parent_thread_id = test.session_configured.thread_id;

    // Collect every child thread announced during the run so we can resolve each one's rollout.
    let mut child_created = test.thread_manager.subscribe_thread_created();

    test.submit_turn("run the uat3 fan-out workflow").await?;

    // Drain the announced child thread ids (the parent turn has completed, so all subagents ran).
    let mut child_ids: HashSet<ThreadId> = HashSet::new();
    while let Ok(child_id) = child_created.try_recv() {
        child_ids.insert(child_id);
    }
    assert_eq!(
        child_ids.len() as u64,
        N,
        "exactly one child thread was announced per subagent"
    );

    let mut rollout_files: HashSet<String> = HashSet::new();
    for child_id in &child_ids {
        let child_thread = test
            .thread_manager
            .get_thread(*child_id)
            .await
            .expect("child thread should be registered");
        child_thread.ensure_rollout_materialized().await;
        child_thread
            .flush_rollout()
            .await
            .expect("child rollout should flush");
        let rollout_path = child_thread
            .rollout_path()
            .expect("child thread should have a rollout path");
        assert!(
            rollout_path.exists(),
            "expected child rollout file at {rollout_path:?}"
        );
        let file_name = rollout_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_string();
        assert!(
            file_name.starts_with("rollout-") && file_name.contains(&child_id.to_string()),
            "rollout file {file_name} should be keyed by the child thread id {child_id}"
        );
        assert!(
            rollout_files.insert(file_name.clone()),
            "each subagent must have its own distinct rollout file (dup: {file_name})"
        );

        let contents = std::fs::read_to_string(&rollout_path)?;
        let lines: Vec<Value> = contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| serde_json::from_str::<Value>(line).expect("rollout line is JSON"))
            .collect();

        assert!(
            rollout_has_response_item(&lines, "reasoning", false),
            "child {child_id} rollout should persist a Reasoning item"
        );
        assert!(
            rollout_has_response_item(&lines, "function_call", false),
            "child {child_id} rollout should persist a FunctionCall item"
        );
        assert!(
            rollout_has_response_item(&lines, "message", true),
            "child {child_id} rollout should persist an assistant AgentMessage item"
        );

        // child_thread_id linkage: the subagent's SessionMeta records both its own id and the
        // workflow parent thread it was spawned from.
        let meta = rollout_session_meta(&lines);
        assert_eq!(
            meta["id"].as_str(),
            Some(child_id.to_string().as_str()),
            "SessionMeta.id should be the child thread id"
        );
        assert_eq!(
            meta["parent_thread_id"].as_str(),
            Some(parent_thread_id.to_string().as_str()),
            "SessionMeta.parent_thread_id should link the subagent back to the workflow thread"
        );
    }

    assert_eq!(
        rollout_files.len() as u64,
        N,
        "exactly one rollout file per subagent"
    );

    Ok(())
}

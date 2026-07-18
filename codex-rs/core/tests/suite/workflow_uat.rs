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
use codex_core::config::RolloutBudgetConfig;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadGoalStatus;
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

/// Recover the plain text a workflow's `text(...)` output carried into the parent's closing model
/// turn (the workflow tool output echoed into that request's `input`), joined across payload lines.
fn workflow_output_full_text(seen: &Arc<Mutex<Vec<Value>>>) -> String {
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
            return workflow_output_text(item);
        }
    }
    panic!("workflow tool output was never sent to the model");
}

/// P2-workflow-registry-reenter — a depth-1 nested `workflow('child', args)` runs the named saved
/// workflow inline in a fresh isolate and returns its top-level result to the parent's awaited
/// promise, with the caller-supplied `args` injected into the child run.
///
/// The child (`child.workflow.js`) is written into `$CODEX_HOME/workflows` so it resolves through
/// the core-workflows registry the host handler consults. It calls `text('child-ran-with:' +
/// args.input)` — proving both that the nested run executed and that it received the parent's args —
/// and the parent workflow returns that value verbatim via `text(await workflow('child', ...))`.
/// The engine artifact is the parent workflow tool output echoed into the model's closing turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_workflow_reenters_and_returns_child_result() -> Result<()> {
    let server = responses::start_mock_server().await;

    // Parent workflow: re-enter the saved `child` workflow one level deep and return its result.
    let parent_source = r#"export const meta = { name: 'uat10parent', description: 'nested re-enter' };
text(await workflow('child', { input: 'hi-from-parent' }));
"#;

    // No subagent turns are expected (neither the parent nor the child spawns an agent); a stub keeps
    // the router shape.
    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_completed(&format!("resp-sub-{ordinal}")),
        ])
    });
    let seen = mount_workflow_router(&server, parent_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            // Save the `child` workflow into `$CODEX_HOME/workflows` so the host handler's registry
            // resolves `workflow('child', ...)` to it.
            let workflows_dir = config.codex_home.as_path().join("workflows");
            std::fs::create_dir_all(&workflows_dir).expect("create workflows dir");
            std::fs::write(
                workflows_dir.join("child.workflow.js"),
                "export const meta = { name: 'child', description: 'nested child' };\n\
                 text('child-ran-with:' + args.input);\n",
            )
            .expect("write child workflow");
        })
        .build(&server)
        .await?;
    test.submit_turn("run the nested workflow").await?;

    let output = workflow_output_full_text(&seen);
    assert!(
        output.contains("child-ran-with:hi-from-parent"),
        "the parent's awaited workflow() promise must carry the nested child run's result \
         (with the caller-supplied args injected); got: {output}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// UAT-10 — nested `workflow()` one level, depth-2 rejected (spec §4/§6, §14.4).
//
// These gates drive the `workflow()` one-level nesting guard end-to-end on the
// hermetic fixture lane. `workflow('name', args)` re-enters a SAVED workflow one
// level deep; a second nesting level is refused by the shared depth gate
// (`next_spawn_depth` + `exceeds_thread_spawn_depth_limit` mapped onto
// `agent_max_depth = 1`, host-side `admit_nested_workflow_depth`) and surfaces as
// a JS throw on the inner `workflow()` promise — never a silent hang. Both tests
// assert on ENGINE ARTIFACTS: the workflow tool output echoed into the model's
// closing turn (depth-1 ran; the depth-2 attempt was rejected with the guard's
// message) and, for depth-1, the nested run's own spawned subagent rollout
// (`SessionMeta.parent_thread_id` linking the child run's agent back into the run
// tree). The depth-guard decision itself is unit-tested at the registry primitive
// (`registry_tests::workflow_one_level_nesting_admits_depth_one_rejects_depth_two`)
// and the host guard (`workflow_handler::nested_workflow_depth_admits_one_level_and_rejects_two`),
// and the `parent_run_id` ledger recording at
// `workflow_handler::workflow_run_ledger_records_parent_linkage`.
// ---------------------------------------------------------------------------

/// UAT-10 (a) — a depth-1 nested `workflow()` runs, receives the caller args, and its
/// nested run spawns a real registered subagent whose rollout links back into the run tree.
///
/// The top-level (depth-0) workflow calls `workflow('linkchild', { input })`; the saved
/// `linkchild` workflow (depth 1, admitted by the one-level guard) both threads the parent's
/// `args.input` into its result AND spawns an `agent()` — so the nested run provably re-entered
/// the runtime one level deep and its subagent accounting flows through the registering path. The
/// engine artifacts are (1) the parent's workflow tool output carrying the child's result with the
/// injected args, and (2) the nested subagent's rollout `SessionMeta.parent_thread_id`, which links
/// the child run's agent back to the workflow thread (the externally-observable manifestation of the
/// nested run's parent linkage; the `parent_run_id` ledger edge itself is unit-tested).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat10_nested_workflow_depth_one_runs_and_links_child_agent() -> Result<()> {
    let server = responses::start_mock_server().await;

    // Top-level workflow: re-enter the saved `linkchild` workflow one level deep and return its
    // result verbatim.
    let parent_source = r#"export const meta = { name: 'uat10linkparent', description: 'depth-1 nested run' };
text(await workflow('linkchild', { input: 'from-parent' }));
"#;

    // The nested run's single subagent turn returns a self-identifying completed message.
    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_assistant_message(
                &format!("msg-sub-{ordinal}"),
                &format!("sub-done-{ordinal}"),
            ),
            ev_completed(&format!("resp-sub-{ordinal}")),
        ])
    });
    let seen = mount_workflow_router(&server, parent_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            // Save the depth-1 `linkchild` workflow: it spawns one subagent (proving the nested run
            // re-enters and its agent accounting flows) and returns a marker carrying the caller args.
            let workflows_dir = config.codex_home.as_path().join("workflows");
            std::fs::create_dir_all(&workflows_dir).expect("create workflows dir");
            std::fs::write(
                workflows_dir.join("linkchild.workflow.js"),
                "export const meta = { name: 'linkchild', description: 'nested child spawns agent' };\n\
                 const r = await agent('WFUAT_SUB_0 nested child work');\n\
                 text('child-linked:' + args.input + ':' + r);\n",
            )
            .expect("write linkchild workflow");
        })
        .build(&server)
        .await?;
    let parent_thread_id = test.session_configured.thread_id;

    // Announce every child thread the run creates so we can resolve the nested subagent's rollout.
    let mut child_created = test.thread_manager.subscribe_thread_created();

    test.submit_turn("run the depth-1 nested workflow").await?;

    // Engine artifact 1: depth-1 nested run executed and returned its result (with args injected)
    // to the parent's awaited promise — proof the run did not silently hang.
    let output = workflow_output_full_text(&seen);
    assert!(
        output.contains("child-linked:from-parent:sub-done-0"),
        "the depth-1 nested workflow() must run, receive the caller args, and return its result \
         (including its own subagent's output); got: {output}"
    );

    // Engine artifact 2: the nested run's subagent is a real registered thread whose rollout links
    // back to the workflow thread — the observable parent linkage for the depth-1 nested run.
    let mut child_ids: HashSet<ThreadId> = HashSet::new();
    while let Ok(child_id) = child_created.try_recv() {
        child_ids.insert(child_id);
    }
    assert_eq!(
        child_ids.len(),
        1,
        "the depth-1 nested run spawns exactly one subagent thread"
    );
    let child_id = child_ids.into_iter().next().expect("one child thread");
    let child_thread = test
        .thread_manager
        .get_thread(child_id)
        .await
        .expect("nested subagent thread should be registered");
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("nested subagent rollout should flush");
    let rollout_path = child_thread
        .rollout_path()
        .expect("nested subagent should have a rollout path");
    let contents = std::fs::read_to_string(&rollout_path)?;
    let lines: Vec<Value> = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("rollout line is JSON"))
        .collect();
    let meta = rollout_session_meta(&lines);
    assert_eq!(
        meta["parent_thread_id"].as_str(),
        Some(parent_thread_id.to_string().as_str()),
        "the nested run's subagent must link back to the workflow thread (run-tree parentage)"
    );

    Ok(())
}

/// UAT-10 (b) — a second `workflow()` nesting level is rejected by the depth guard and surfaces as
/// a catchable error on the inner promise, not a silent hang.
///
/// The top-level (depth-0) workflow calls `workflow('mid', args)`; the saved `mid` workflow (depth
/// 1, admitted) then calls `workflow('leaf', args)` — which would create a *second* nesting level
/// (depth 2) and is refused by the one-level guard. `mid` wraps the deeper call in `try/catch` and
/// returns the caught error text, and `leaf` (which must never run) writes a sentinel. The engine
/// artifact is the parent's workflow tool output: it shows `mid` (depth 1) ran to completion, the
/// depth-2 `workflow('leaf')` attempt was REJECTED with the guard's one-level-nesting message
/// (surfaced as a JS throw, caught by the script), and the `leaf` body never executed. The whole
/// turn completing at all is itself the "not a silent hang" assertion.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat10_second_nesting_level_rejected_as_error() -> Result<()> {
    let server = responses::start_mock_server().await;

    let parent_source = r#"export const meta = { name: 'uat10rejparent', description: 'depth-2 rejection' };
text(await workflow('mid', { input: 'hi' }));
"#;

    // No subagent turns are expected in this scenario; a stub keeps the router shape.
    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_completed(&format!("resp-sub-{ordinal}")),
        ])
    });
    let seen = mount_workflow_router(&server, parent_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            let workflows_dir = config.codex_home.as_path().join("workflows");
            std::fs::create_dir_all(&workflows_dir).expect("create workflows dir");
            // Depth-1 `mid`: runs, then attempts a depth-2 `workflow('leaf')` and reports the
            // rejection it catches.
            std::fs::write(
                workflows_dir.join("mid.workflow.js"),
                "export const meta = { name: 'mid', description: 'nesting middle' };\n\
                 let outcome;\n\
                 try {\n\
                 \x20 await workflow('leaf', { input: 'deeper' });\n\
                 \x20 outcome = 'leaf-admitted-UNEXPECTED';\n\
                 } catch (e) {\n\
                 \x20 outcome = 'leaf-rejected:' + String(e);\n\
                 }\n\
                 text('mid-depth1-ran; ' + outcome);\n",
            )
            .expect("write mid workflow");
            // Depth-2 `leaf`: must never execute (its call is rejected before re-entry).
            std::fs::write(
                workflows_dir.join("leaf.workflow.js"),
                "export const meta = { name: 'leaf', description: 'too deep' };\n\
                 text('leaf-should-never-run');\n",
            )
            .expect("write leaf workflow");
        })
        .build(&server)
        .await?;
    test.submit_turn("run the depth-2 nested workflow").await?;

    let output = workflow_output_full_text(&seen);
    // The depth-1 middle workflow ran to completion (the turn did not hang before returning).
    assert!(
        output.contains("mid-depth1-ran"),
        "the depth-1 workflow must run to completion; got: {output}"
    );
    // The depth-2 attempt surfaced as a catchable error on the inner workflow() promise.
    assert!(
        output.contains("leaf-rejected:"),
        "the depth-2 workflow() attempt must reject as a catchable JS error; got: {output}"
    );
    // The rejection carries the one-level nesting-guard message (not some unrelated failure).
    assert!(
        output.contains("one-level nesting limit") || output.contains("nest only one level"),
        "the rejection must be the one-level nesting guard's error; got: {output}"
    );
    // The guard fired BEFORE re-entry: the leaf body never executed and the deeper call was never
    // mistakenly admitted.
    assert!(
        !output.contains("leaf-should-never-run"),
        "the rejected depth-2 workflow body must never execute; got: {output}"
    );
    assert!(
        !output.contains("UNEXPECTED"),
        "the depth-2 workflow() must reject, not resolve; got: {output}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// UAT-7 — `pipeline()` no-barrier (spec §5, §14.4)
//
// These gates drive a real `pipeline(items, ...stages)` workflow end-to-end on the
// hermetic fixture lane. Each pipeline stage issues an `agent()` call whose prompt
// carries a `WFUAT_SUB_<code>` marker with `code = item_index * 10 + stage_index`, so
// every subagent turn is a distinct registered thread that hits the fixture model with a
// self-identifying marker. The tests assert on ENGINE ARTIFACTS — the ordered arrival log
// of subagent model requests and the workflow's own position-preserving return array —
// never on model free text.
//
// The no-barrier semantic is observed by holding one item's first-stage response behind a
// deterministic fixture LATCH (NOT a wall-clock delay): item 1's stage-0 subagent is kept
// looping on tool-call round-trips until the fixture observes item 0's *last*-stage request
// arrive, then released. So while item 1 is pinned in stage 0, its sibling advances through
// every stage, and the sibling's last-stage request provably arrives before item 1's
// second-stage request. That ordering is impossible under a per-stage barrier (which would
// force every item through stage 0 before any item entered stage 1) and falls straight out of
// the per-item independent promise chains. Because the latch releases on an OBSERVED ENGINE
// EVENT (item 0's stage-2 request landing) rather than after a fixed sleep, the staggered
// ordering is deterministic on arbitrarily slow CI — a slow machine simply loops item 1's
// stage-0 subagent a few more times before the same release fires.
// ---------------------------------------------------------------------------

/// Pipeline code marker helpers: `code = item_index * 10 + stage_index` encodes both the
/// item and the stage into the single integer the shared [`subagent_ordinal`] parser
/// recovers, so the existing marker-routing machinery serves the two-dimensional
/// (item, stage) fan-out without new plumbing.
fn pipeline_code(item: u64, stage: u64) -> u64 {
    item * 10 + stage
}

/// Arrival index (position in the ordered `seen` request log) of the first subagent
/// request carrying the given pipeline `code`, or `None` if that stage never dispatched.
fn subagent_arrival_index(seen: &Arc<Mutex<Vec<Value>>>, code: u64) -> Option<usize> {
    seen.lock()
        .unwrap()
        .iter()
        .position(|body| subagent_ordinal(body) == Some(code))
}

/// Safety cap on how many tool-call round-trips the latched subagent may loop through before it
/// is force-released, so a bug that prevents the release condition from ever firing (e.g. a
/// per-stage barrier that deadlocks item 0 behind item 1) fails the ordering assertion loudly
/// instead of hanging forever. Under the correct no-barrier engine the release fires after item 0's
/// handful of quick stages, well under this cap.
const PIPELINE_LATCH_MAX_ROUNDS: usize = 64;

/// A routing responder for the pipeline UAT: identical dispatch shape to [`WorkflowRouter`]
/// (subagent marker -> per-code SSE; parent open -> `workflow` custom-tool call; parent
/// close -> final message), but it holds `latched_code`'s subagent in an early stage using a
/// DETERMINISTIC latch rather than a wall-clock delay.
///
/// The latched subagent is kept looping on `update_plan` tool-call round-trips (each round-trip
/// re-enters this responder) until the fixture observes `release_code`'s request arrive in the
/// shared `seen` log — the engine artifact that item 0 reached its final stage. Only then is the
/// latched subagent handed its final assistant message. Because release is gated on an observed
/// request (not elapsed time), the staggered ordering the test asserts is deterministic on slow CI.
/// Every OTHER subagent turn is a single round-trip returning a completed assistant message.
struct PipelineRouter {
    workflow_source: String,
    /// The subagent code held in an early stage until `release_code` is observed.
    latched_code: u64,
    /// Observing a request for this code in `seen` releases the latched subagent.
    release_code: u64,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl PipelineRouter {
    /// Final single-round-trip subagent turn: a completed assistant message carrying the code.
    fn final_subagent_sse(code: u64) -> String {
        sse(vec![
            ev_response_created(&format!("resp-sub-{code}")),
            ev_assistant_message(&format!("msg-sub-{code}"), &code.to_string()),
            ev_completed(&format!("resp-sub-{code}")),
        ])
    }

    /// A non-terminal subagent round-trip: an `update_plan` tool call that drives the child to
    /// execute it and re-request, so the latched subagent keeps looping (and re-entering this
    /// responder) until the release condition is met.
    fn latch_loop_sse(code: u64, round: usize) -> String {
        let plan_args = json!({
            "plan": [{ "step": format!("latch-{code}-{round}"), "status": "in_progress" }],
        })
        .to_string();
        sse(vec![
            ev_response_created(&format!("resp-sub-{code}-{round}")),
            ev_function_call(
                &format!("call-latch-{code}-{round}"),
                "update_plan",
                &plan_args,
            ),
            ev_completed(&format!("resp-sub-{code}-{round}")),
        ])
    }
}

impl Respond for PipelineRouter {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = request_body_json(request);
        let mut seen = self.seen.lock().unwrap();
        seen.push(body.clone());

        if let Some(code) = subagent_ordinal(&body) {
            if code != self.latched_code {
                return sse_response(Self::final_subagent_sse(code));
            }
            // Latched subagent: release only once item 0's final-stage request has arrived.
            let released = seen
                .iter()
                .any(|seen_body| subagent_ordinal(seen_body) == Some(self.release_code));
            if released {
                return sse_response(Self::final_subagent_sse(code));
            }
            let round = seen
                .iter()
                .filter(|seen_body| subagent_ordinal(seen_body) == Some(self.latched_code))
                .count();
            if round >= PIPELINE_LATCH_MAX_ROUNDS {
                // Force-release: the release condition never fired (a barrier bug). Let the turn
                // finish so the ordering assertion can catch the violation loudly.
                return sse_response(Self::final_subagent_sse(code));
            }
            return sse_response(Self::latch_loop_sse(code, round));
        }

        if contains_workflow_tool_output(&body) {
            return sse_response(sse(vec![
                ev_response_created("resp-parent-final"),
                ev_assistant_message("msg-parent-final", "pipeline done"),
                ev_completed("resp-parent-final"),
            ]));
        }

        sse_response(sse(vec![
            ev_response_created("resp-parent-open"),
            responses::ev_custom_tool_call(WORKFLOW_CALL_ID, "workflow", &self.workflow_source),
            ev_completed("resp-parent-open"),
        ]))
    }
}

/// Mount the [`PipelineRouter`] on the mock server and return the shared request log. The
/// `latched_code` subagent is held in its stage until `release_code`'s request is observed.
async fn mount_pipeline_router(
    server: &MockServer,
    workflow_source: &str,
    latched_code: u64,
    release_code: u64,
) -> Arc<Mutex<Vec<Value>>> {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let router = PipelineRouter {
        workflow_source: workflow_source.to_string(),
        latched_code,
        release_code,
        seen: Arc::clone(&seen),
    };
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(router)
        .mount(server)
        .await;
    seen
}

/// UAT-7 (a) — `pipeline()` no-barrier staggered progress.
///
/// Two items each thread through three stages; every stage issues an `agent()` call. Item
/// 1's stage-0 response is held behind a fixture latency, so while item 1 is stalled before
/// it can leave stage 0, item 0 runs all three stages to completion. The engine artifact is
/// the ordered subagent-request arrival log: item 0's stage-2 (last) request provably
/// arrives before item 1's stage-1 request — item A reached its final stage while item B was
/// still stuck in stage 0. A per-stage barrier could never produce that ordering. The
/// workflow's own return stays position-preserving (`[0, 1]`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat7_pipeline_no_barrier_staggered_progress() -> Result<()> {
    let server = responses::start_mock_server().await;

    // Each stage `n` issues `agent("WFUAT_SUB_<item*10+n> ...")` and returns the original
    // item index unchanged, so the item identity threads through the whole chain and the
    // per-stage agent request self-identifies via its marker.
    let marker = SUBAGENT_MARKER;
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat7', description: 'pipeline no-barrier staggering' }};
const stage = (n) => async (x) => {{
  await agent("{marker}" + (x * 10 + n) + " item " + x + " stage " + n);
  return x;
}};
const results = await pipeline([0, 1], stage(0), stage(1), stage(2));
text(JSON.stringify(results));
"#,
    );

    // Latch item 1's stage-0 subagent until item 0's final-stage request is observed, so item 0's
    // entire chain provably finishes while item 1 is still pinned in stage 0 — deterministically,
    // with no wall-clock dependency.
    let seen = mount_pipeline_router(
        &server,
        &workflow_source,
        pipeline_code(1, 0),
        pipeline_code(0, 2),
    )
    .await;

    let test = workflow_builder(&server.uri())
        // Raise the per-session registry ceiling so the run's six lifetime subagents (2
        // items x 3 stages), which stay registered for the run, never hit the hard backstop.
        .with_config(|config| {
            config.multi_agent_v2.max_concurrent_threads_per_session = 32;
        })
        .build(&server)
        .await?;
    test.submit_turn("run the uat7 pipeline workflow").await?;

    // Position-preserving return: each item threads its index through every stage.
    let results = workflow_return_array(&seen);
    assert_eq!(results, vec![json!(0), json!(1)], "pipeline return [0, 1]");

    // Every stage of both items dispatched a subagent request (all six agents ran).
    for item in 0..2 {
        for stage in 0..3 {
            let code = pipeline_code(item, stage);
            assert!(
                subagent_arrival_index(&seen, code).is_some(),
                "stage request for code {code} (item {item} stage {stage}) must have dispatched"
            );
        }
    }

    // No-barrier engine artifact: item 0's LAST-stage request arrives before item 1's
    // SECOND-stage request. Item A reached stage 3 while item B was still in stage 1.
    let a_last_stage =
        subagent_arrival_index(&seen, pipeline_code(0, 2)).expect("item 0 stage-2 request arrived");
    let b_second_stage =
        subagent_arrival_index(&seen, pipeline_code(1, 1)).expect("item 1 stage-1 request arrived");
    assert!(
        a_last_stage < b_second_stage,
        "no barrier: item 0 must reach its final stage (arrival {a_last_stage}) before item 1 \
         leaves stage 0 to issue its stage-1 request (arrival {b_second_stage})"
    );

    Ok(())
}

/// UAT-7 (b) — a pipeline stage throw drops only that item to `null`.
///
/// Both items thread through three stages; item 1's stage-1 function throws after its agent
/// call. The pipeline's per-item `.catch(() => null)` drops item 1 to `null` at its slot and
/// short-circuits its remaining stages (item 1's stage-2 agent never dispatches), while item
/// 0 runs all three stages unaffected. Asserted on engine artifacts: the position-preserving
/// return `[0, null]`, item 0's full stage coverage, and the absence of item 1's stage-2
/// request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat7_pipeline_stage_throw_isolates_to_item() -> Result<()> {
    let server = responses::start_mock_server().await;

    let marker = SUBAGENT_MARKER;
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat7throw', description: 'pipeline stage-throw isolation' }};
const stage = (n) => async (x) => {{
  await agent("{marker}" + (x * 10 + n) + " item " + x + " stage " + n);
  if (x === 1 && n === 1) throw new Error('stage-boom');
  return x;
}};
const results = await pipeline([0, 1], stage(0), stage(1), stage(2));
text(JSON.stringify(results));
"#,
    );

    let subagent: SubagentScript = Box::new(|code, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{code}")),
            ev_assistant_message(&format!("msg-sub-{code}"), &code.to_string()),
            ev_completed(&format!("resp-sub-{code}")),
        ])
    });

    let seen = mount_workflow_router(&server, &workflow_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            config.multi_agent_v2.max_concurrent_threads_per_session = 32;
        })
        .build(&server)
        .await?;
    test.submit_turn("run the uat7 stage-throw workflow")
        .await?;

    // The throwing item's slot is `null`; the sibling holds its value — position-preserving.
    let results = workflow_return_array(&seen);
    assert_eq!(
        results,
        vec![json!(0), Value::Null],
        "stage throw drops only item 1 to null; item 0 keeps its position and value"
    );

    // Item 0 (sibling) advanced through every stage, unaffected by item 1's throw.
    for stage in 0..3 {
        let code = pipeline_code(0, stage);
        assert!(
            subagent_arrival_index(&seen, code).is_some(),
            "sibling item 0 stage {stage} (code {code}) must have run"
        );
    }

    // Item 1 ran up to and including the throwing stage (0 and 1) ...
    assert!(
        subagent_arrival_index(&seen, pipeline_code(1, 0)).is_some(),
        "item 1 stage 0 ran before the throw"
    );
    assert!(
        subagent_arrival_index(&seen, pipeline_code(1, 1)).is_some(),
        "item 1 stage 1 ran and then threw"
    );
    // ... but the throw short-circuited its remaining stage: stage 2 never dispatched.
    assert!(
        subagent_arrival_index(&seen, pipeline_code(1, 2)).is_none(),
        "item 1 stage 2 must NOT dispatch after the stage-1 throw dropped the item to null"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Budget pre-admission hard ceiling (spec §5 admission-order step 1, §8;
// `P2-budget-pre-admission-throw`).
//
// A metered workflow run drives `agent()` calls sequentially; the shared,
// tree-wide `RolloutBudget` accrues each subagent turn's fixture token usage. The
// pre-admission gate in `CoreTurnHost::spawn_agent` refuses the FIRST `agent()`
// call whose admission-time `remaining()` is `<= 0` by returning
// `AgentSpawnOutcome::Rejected("BudgetExceeded")`, which the isolate surfaces as a
// synchronous JS throw (not the death-is-null `null`). The engine artifacts are
// the workflow's own return array (recording the exact ordinal that threw and the
// error text) and the subagent-request arrival log (the throwing ordinal never
// dispatches a model request, and no later subagent runs after the throw).
// ---------------------------------------------------------------------------

/// UAT-5 — budget hard ceiling: `agent()` throws `BudgetExceeded` at the exact ordinal where
/// `remaining()` first hits `<= 0`, and no further subagents spawn.
///
/// The run is metered by a `RolloutBudget` with `limit_tokens = 250` counting input tokens 1:1
/// (`prefill_token_weight = 1.0`), and each subagent turn reports 100 input tokens via
/// `ev_completed_with_tokens`. The workflow issues `agent()` calls sequentially in a `try/catch`
/// loop, so budget accrues between calls and the throw is deterministic:
/// - ordinal 0 (pre-check remaining 250) spawns; after it, spent 100, remaining 150.
/// - ordinal 1 (remaining 150) spawns; after it, spent 200, remaining 50.
/// - ordinal 2 (remaining 50) spawns; its turn pushes spent to 300 (the in-flight backstop aborts
///   *that* child's turn -> it resolves to `null`), leaving remaining 0.
/// - ordinal 3 (remaining 0) is refused pre-admission -> `agent()` THROWS `BudgetExceeded`.
///
/// The loop catches the throw at ordinal 3 and breaks, so ordinal 4 never runs. Assertions ride the
/// workflow's own return (ordinals 0-2 ran; ordinal 3 threw with `BudgetExceeded`), the arrival
/// log (a model request dispatched for 0/1/2 but never for the pre-admission-refused ordinal 3, nor
/// for ordinal 4), and the §8 reporting channel: a `ThreadGoalUpdated` carrying
/// `ThreadGoalStatus::BudgetLimited` surfaces at the ceiling (observed via a non-competing event
/// tap). The high `max_concurrent_threads_per_session` guarantees the lifetime cap is not the
/// limiting factor, so the only possible throw is the budget ceiling (step 1 precedes the lifetime
/// CAS of step 2). Fixed `ev_completed_with_tokens` counts make the ceiling-throw ordinal (3)
/// byte-identical on every run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat5_budget_pre_admission_throw_at_ceiling() -> Result<()> {
    let server = responses::start_mock_server().await;

    let marker = SUBAGENT_MARKER;
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat5', description: 'budget hard ceiling' }};
const results = [];
for (let i = 0; i < 5; i++) {{
  try {{
    await agent("{marker}" + i + " do budgeted work");
    results.push(i);
  }} catch (e) {{
    results.push({{ threwAt: i, error: String(e) }});
    break;
  }}
}}
text(JSON.stringify(results));
"#,
    );

    // Every subagent turn is a single round-trip that reports 100 input tokens against the budget.
    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_assistant_message(&format!("msg-sub-{ordinal}"), &format!("done-{ordinal}")),
            ev_completed_with_tokens(&format!("resp-sub-{ordinal}"), 100),
        ])
    });

    let seen = mount_workflow_router(&server, &workflow_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            // Meter the run: a 250-token ceiling counting input tokens 1:1 (output tokens are 0 in
            // the fixture usage, so `prefill_token_weight = 1.0` is what makes the fixture counts
            // accrue). Shared tree-wide via the session's `AgentControl.rollout_budget`.
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 250,
                reminder_at_remaining_tokens: Vec::new(),
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
            // Raise the registry ceiling so the lifetime cap never fires first: the ONLY throw in
            // this run must be the budget ceiling, proving step 1 precedes the lifetime CAS.
            config.multi_agent_v2.max_concurrent_threads_per_session = 32;
        })
        .build(&server)
        .await?;

    // Tap the workflow session's event stream BEFORE the turn: at the ceiling the §8 reporting half
    // (`build_budget_thread_goal`) emits a `ThreadGoalUpdated` on the EXISTING ThreadGoal channel
    // carrying `ThreadGoalStatus::BudgetLimited` immediately before the pre-admission throw. The tap
    // is non-competing (it does not steal from `submit_turn`'s consumer) and must exist before the
    // turn since a `broadcast::Receiver` only observes events published after `subscribe`.
    let mut events = test.codex.subscribe_events();

    test.submit_turn("run the uat5 budget-ceiling workflow")
        .await?;

    // The workflow's own return: ordinals 0-2 ran (pushed as their index), then ordinal 3 threw.
    let results = workflow_return_array(&seen);
    assert_eq!(
        results.len(),
        4,
        "three admitted agents then one pre-admission throw, then break: {results:?}"
    );
    assert_eq!(results[0], json!(0), "ordinal 0 admitted and ran");
    assert_eq!(results[1], json!(1), "ordinal 1 admitted and ran");
    assert_eq!(
        results[2],
        json!(2),
        "ordinal 2 admitted and ran (overshoot turn)"
    );

    // The throw lands at the EXACT ordinal where remaining() first hits <= 0, and carries the
    // BudgetExceeded reason (a genuine throw, not the death-is-null `null`).
    assert_eq!(
        results[3]["threwAt"],
        json!(3),
        "the pre-admission throw lands at ordinal 3 (remaining() == 0): {results:?}"
    );
    let error_text = results[3]["error"].as_str().unwrap_or_default();
    assert!(
        error_text.contains("BudgetExceeded"),
        "the throw must surface BudgetExceeded, got: {error_text:?}"
    );
    assert!(
        !error_text.contains("AgentCapReached"),
        "the throw must be the budget ceiling, not the lifetime cap: {error_text:?}"
    );

    // Arrival log: ordinals 0/1/2 each dispatched a model request; the pre-admission-refused
    // ordinal 3 never spawned, and ordinal 4 never ran (the loop broke after the throw).
    for ordinal in 0..3 {
        assert!(
            subagent_arrival_index(&seen, ordinal).is_some(),
            "ordinal {ordinal} was admitted and must have dispatched a subagent request"
        );
    }
    assert!(
        subagent_arrival_index(&seen, 3).is_none(),
        "ordinal 3 threw pre-admission and must NEVER dispatch a subagent request"
    );
    assert!(
        subagent_arrival_index(&seen, 4).is_none(),
        "no further subagents spawn after the throw: ordinal 4 must never run"
    );

    // §8 reporting: drain the non-competing event tap and collect every budget ThreadGoal status.
    // All of the run's `ThreadGoalUpdated` events were published to the broadcast (capacity 1024,
    // far above this run's event count) before `TurnComplete`, so they are buffered in the receiver
    // and `try_recv` observes them without loss.
    let mut goal_statuses = Vec::new();
    loop {
        match events.try_recv() {
            Ok(event) => {
                if let EventMsg::ThreadGoalUpdated(updated) = event.msg {
                    goal_statuses.push(updated.goal.status);
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
            | Err(tokio::sync::broadcast::error::TryRecvError::Closed) => break,
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => continue,
        }
    }
    // The budget-limited condition surfaces at the ceiling through the existing ThreadGoal channel
    // (no new protocol types): the ordinal-3 pre-admission gate emits `BudgetLimited` before it
    // throws.
    assert!(
        goal_statuses.contains(&ThreadGoalStatus::BudgetLimited),
        "the budget ceiling must surface ThreadGoalStatus::BudgetLimited on the ThreadGoal \
         channel; observed statuses: {goal_statuses:?}"
    );

    Ok(())
}

/// UAT-5 (JS introspection) — the in-process `budget` global is LIVE, not static.
///
/// This gate covers the in-process-lane readout gap: the isolate's `budget.spent()` /
/// `budget.remaining()` native functions (and the `budget.total` property) must forward to the
/// session's shared, tree-wide `RolloutBudget` so a workflow can loop until its budget is nearly
/// exhausted. Before the in-process delegate returned a live handle, the broker inherited the
/// default `budget_handle() -> None`, so on THIS lane (`Feature::CodeModeHost` disabled) the globals
/// reported static values (`total`/`spent`/`remaining` all `0`, since a top-level run carries no
/// `args.budget.total`) even while enforcement metered real spend — which breaks the
/// loop-until-budget authoring pattern.
///
/// The run is metered by a session `RolloutBudget` (`limit_tokens = 1000`, `prefill_token_weight =
/// 1.0`), and each subagent turn reports 100 input tokens via `ev_completed_with_tokens`. The
/// workflow reads the globals before any spawn and again after each of two sequential `agent()`
/// calls, returning the readings as its result. The readings must reflect live accrual:
/// - start: `total 1000`, `spent 0`, `remaining 1000`.
/// - after ordinal 0: `spent 100`, `remaining 900`.
/// - after ordinal 1: `spent 200`, `remaining 800`.
///
/// With the pre-fix static `None` handle every reading would instead be `total 0` / `spent 0` /
/// `remaining 0`, so the `spent == 100/200` and `total == 1000` assertions are what prove the handle
/// is live on the in-process lane. Engine artifacts (a dispatched subagent request per admitted
/// ordinal) are asserted too, so the readings are anchored to real spawns rather than free text.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat5_in_process_budget_global_reports_live_accrual() -> Result<()> {
    let server = responses::start_mock_server().await;

    let marker = SUBAGENT_MARKER;
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat5live', description: 'live budget introspection' }};
const readings = [];
const read = (phase) => readings.push({{
  phase,
  total: budget.total,
  spent: budget.spent(),
  remaining: budget.remaining(),
}});
read('start');
await agent("{marker}0 budgeted work");
read('after0');
await agent("{marker}1 budgeted work");
read('after1');
text(JSON.stringify(readings));
"#,
    );

    // Every subagent turn is a single round-trip reporting 100 input tokens against the budget.
    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_assistant_message(&format!("msg-sub-{ordinal}"), &format!("done-{ordinal}")),
            ev_completed_with_tokens(&format!("resp-sub-{ordinal}"), 100),
        ])
    });

    let seen = mount_workflow_router(&server, &workflow_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            // Meter the run with a generous 1000-token ceiling counting input tokens 1:1 so the two
            // sequential agents (100 tokens each) accrue without ever hitting the ceiling. Shared
            // tree-wide via the session's `AgentControl.rollout_budget`.
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 1000,
                reminder_at_remaining_tokens: Vec::new(),
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
            // Keep the lifetime cap out of the way: both agents stay registered for the run.
            config.multi_agent_v2.max_concurrent_threads_per_session = 32;
        })
        .build(&server)
        .await?;

    test.submit_turn("run the uat5 live-budget workflow")
        .await?;

    // The workflow's own return carries the three readings the isolate took.
    let readings = workflow_return_array(&seen);
    assert_eq!(
        readings.len(),
        3,
        "the workflow reads the budget at start + after each of two agents: {readings:?}"
    );

    // The `budget.total` property is sourced from the LIVE handle's configured ceiling (1000), not
    // the top-level run's absent `args.budget.total` (which would read 0 without the handle).
    for reading in &readings {
        assert_eq!(
            reading["total"],
            json!(1000),
            "budget.total must report the live session ceiling on the in-process lane: {reading:?}"
        );
    }

    // Start: nothing spent yet.
    assert_eq!(readings[0]["phase"], json!("start"));
    assert_eq!(readings[0]["spent"], json!(0), "no spend before any agent");
    assert_eq!(readings[0]["remaining"], json!(1000));

    // After ordinal 0: the first subagent's 100 tokens have accrued live.
    assert_eq!(readings[1]["phase"], json!("after0"));
    assert_eq!(
        readings[1]["spent"],
        json!(100),
        "budget.spent() must reflect the first agent's live accrual (not static 0): {readings:?}"
    );
    assert_eq!(readings[1]["remaining"], json!(900));

    // After ordinal 1: tree-wide spend has grown to 200 — the readout is live, not a snapshot.
    assert_eq!(readings[2]["phase"], json!("after1"));
    assert_eq!(
        readings[2]["spent"],
        json!(200),
        "budget.spent() must reflect both agents' cumulative live accrual: {readings:?}"
    );
    assert_eq!(readings[2]["remaining"], json!(800));

    // Engine artifacts: each admitted ordinal actually dispatched a subagent model request, so the
    // readings are anchored to real spawns.
    for ordinal in 0..2 {
        assert!(
            subagent_arrival_index(&seen, ordinal).is_some(),
            "ordinal {ordinal} must have dispatched a subagent request"
        );
    }

    Ok(())
}

/// A `response.completed` SSE event reporting `output_tokens` (the nested-workflow budget uses the
/// pure OUTPUT-weight config, so a subagent must report output — not input — tokens to accrue).
fn ev_completed_with_output_tokens(id: &str, output_tokens: i64) -> Value {
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

/// Count the workflow return-array entries that are plain admitted ordinals (a number) versus the
/// per-element `{ threwAt/rejected, error }` rejection records the `try/catch` produced.
fn partition_admitted_rejected(results: &[Value]) -> (Vec<i64>, Vec<Value>) {
    let mut admitted = Vec::new();
    let mut rejected = Vec::new();
    for value in results {
        match value.as_i64() {
            Some(ordinal) => admitted.push(ordinal),
            None => rejected.push(value.clone()),
        }
    }
    (admitted, rejected)
}

/// UAT-5 (admission order) — the budget ceiling wins over the lifetime cap and does NOT consume a
/// lifetime slot.
///
/// The pre-admission ceiling check (spec §5 step 1) runs in `CoreTurnHost::spawn_agent` BEFORE the
/// scheduler's lifetime CAS (step 2). This test saturates BOTH gates at the same ordinal: a 250-token
/// budget (100 input tokens/turn, `prefill_token_weight = 1.0`) exhausts after ordinals 0/1/2, and the
/// lifetime cap is pinned to exactly 3 (`max_concurrent_threads_per_session = 4` →
/// `effective_agent_max_threads = 3`), so ordinals 0/1/2 also fill every lifetime slot. Ordinal 3 is
/// therefore refused by BOTH gates — and because step 1 precedes step 2, the throw is `BudgetExceeded`,
/// never `AgentCapReached`. That is the admission-order proof: the budget gate fires first and the
/// lifetime slot is never even attempted for ordinal 3.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat5_budget_admission_order_beats_lifetime_cap() -> Result<()> {
    let server = responses::start_mock_server().await;

    let marker = SUBAGENT_MARKER;
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat5order', description: 'budget beats lifetime cap' }};
const results = [];
for (let i = 0; i < 6; i++) {{
  try {{
    await agent("{marker}" + i + " budgeted work");
    results.push(i);
  }} catch (e) {{
    results.push({{ threwAt: i, error: String(e) }});
    break;
  }}
}}
text(JSON.stringify(results));
"#,
    );

    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_assistant_message(&format!("msg-sub-{ordinal}"), &format!("done-{ordinal}")),
            ev_completed_with_tokens(&format!("resp-sub-{ordinal}"), 100),
        ])
    });

    let seen = mount_workflow_router(&server, &workflow_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 250,
                reminder_at_remaining_tokens: Vec::new(),
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
            // effective_agent_max_threads = max_concurrent_threads_per_session - 1 = 3, so ordinals
            // 0/1/2 fill every lifetime slot: ordinal 3 would ALSO hit the lifetime cap were the
            // budget gate not checked first.
            config.multi_agent_v2.max_concurrent_threads_per_session = 4;
        })
        .build(&server)
        .await?;

    test.submit_turn("run the uat5 admission-order workflow")
        .await?;

    let results = workflow_return_array(&seen);
    assert_eq!(
        results.len(),
        4,
        "three admitted agents then one throw, then break: {results:?}"
    );
    assert_eq!(results[0], json!(0));
    assert_eq!(results[1], json!(1));
    assert_eq!(results[2], json!(2));
    assert_eq!(
        results[3]["threwAt"],
        json!(3),
        "the throw lands at the ordinal where both gates are saturated: {results:?}"
    );
    let error_text = results[3]["error"].as_str().unwrap_or_default();
    // Admission order: the budget pre-check (step 1) fires before the lifetime CAS (step 2), so the
    // budget wins even though the lifetime cap is ALSO saturated at this ordinal.
    assert!(
        error_text.contains("BudgetExceeded"),
        "the budget gate must win the admission order, got: {error_text:?}"
    );
    assert!(
        !error_text.contains("AgentCapReached"),
        "the lifetime cap must NOT be the throw at ordinal 3 (budget precedes it): {error_text:?}"
    );

    // The budget gate refused ordinal 3 without dispatching a model request (and without consuming a
    // lifetime slot, since step 2 was never reached).
    for ordinal in 0..3 {
        assert!(
            subagent_arrival_index(&seen, ordinal).is_some(),
            "ordinal {ordinal} was admitted and must have dispatched a subagent request"
        );
    }
    assert!(
        subagent_arrival_index(&seen, 3).is_none(),
        "ordinal 3 was refused pre-admission and must never dispatch a subagent request"
    );

    Ok(())
}

/// UAT-5 (concurrency) — a `parallel()` fan-out can NOT exceed the budget ceiling by more than the
/// documented bound.
///
/// This is the concurrency counterpart to the sequential ceiling test and the exact defect the
/// pre-fix unreserved `remaining()` read allowed: N agents fanned out at once each read headroom
/// before any child recorded usage, so ALL N were admitted and the ceiling was blown by up to N
/// turns. With the race-free reservation gate, concurrent admissions serialize on the shared budget
/// lock, so the number of subagents that actually DISPATCH a model request is bounded by
/// `ceil(limit / turn)` (here `ceil(250 / 100) = 3`) — never the fan-out width (8). Every refused
/// element throws `BudgetExceeded`, caught per-element by the workflow's `try/catch`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat5_parallel_fanout_cannot_exceed_budget_ceiling() -> Result<()> {
    const FANOUT: usize = 8;
    // ceil(limit / per-turn) = ceil(250 / 100): at most this many turns may accrue before the
    // ceiling closes, so at most this many subagents may dispatch.
    const MAX_DISPATCHED: usize = 3;

    let server = responses::start_mock_server().await;

    let marker = SUBAGENT_MARKER;
    let workflow_source = format!(
        r#"export const meta = {{ name: 'uat5par', description: 'parallel fan-out ceiling' }};
const thunks = [];
for (let i = 0; i < {FANOUT}; i++) {{
  thunks.push((function (k) {{
    return async () => {{
      try {{
        await agent("{marker}" + k + " budgeted work");
        return k;
      }} catch (e) {{
        return {{ rejected: k, error: String(e) }};
      }}
    }};
  }})(i));
}}
const results = await parallel(thunks);
text(JSON.stringify(results));
"#,
    );

    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_assistant_message(&format!("msg-sub-{ordinal}"), &format!("done-{ordinal}")),
            ev_completed_with_tokens(&format!("resp-sub-{ordinal}"), 100),
        ])
    });

    let seen = mount_workflow_router(&server, &workflow_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 250,
                reminder_at_remaining_tokens: Vec::new(),
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
            // High registry ceiling so the LIFETIME cap is never the limiter: the ONLY thing that may
            // bound this fan-out is the budget reservation gate.
            config.multi_agent_v2.max_concurrent_threads_per_session = 64;
        })
        .build(&server)
        .await?;

    test.submit_turn("run the uat5 parallel fan-out workflow")
        .await?;

    let results = workflow_return_array(&seen);
    assert_eq!(
        results.len(),
        FANOUT,
        "position-preserving: one slot per fan-out element: {results:?}"
    );
    let (admitted, rejected) = partition_admitted_rejected(&results);

    // The engine artifact: how many subagent MODEL REQUESTS actually dispatched. This is the ground
    // truth for "did the fan-out blow past the ceiling". Under the fix it is bounded by ceil(limit /
    // turn); the pre-fix hole let all FANOUT through.
    let dispatched = (0..FANOUT as u64)
        .filter(|&code| subagent_arrival_index(&seen, code).is_some())
        .count();
    assert!(
        (1..=MAX_DISPATCHED).contains(&dispatched),
        "the budget reservation must bound dispatched subagents to at most ceil(limit/turn) = \
         {MAX_DISPATCHED}, got {dispatched}"
    );
    assert!(
        dispatched < FANOUT,
        "the ceiling must actually bite: strictly fewer than the {FANOUT}-wide fan-out may dispatch \
         (pre-fix, all {FANOUT} did), got {dispatched}"
    );

    // Every admitted element dispatched exactly one request; no rejected element dispatched — so the
    // admitted count equals the dispatched count (no over-dispatch slipped past the gate).
    assert_eq!(
        admitted.len(),
        dispatched,
        "admitted elements must equal dispatched model requests: {results:?}"
    );
    assert_eq!(
        rejected.len(),
        FANOUT - admitted.len(),
        "every non-admitted element must be a caught rejection: {results:?}"
    );
    // Each admitted element that dispatched must have its own model request in the arrival log.
    for ordinal in &admitted {
        assert!(
            subagent_arrival_index(&seen, *ordinal as u64).is_some(),
            "admitted ordinal {ordinal} must have dispatched a request"
        );
    }
    // Every refused element threw the BUDGET ceiling (not the lifetime cap, which we sized out).
    for record in &rejected {
        let error_text = record["error"].as_str().unwrap_or_default();
        assert!(
            error_text.contains("BudgetExceeded"),
            "a fan-out element refused by the ceiling must throw BudgetExceeded, got: {error_text:?}"
        );
        assert!(
            !error_text.contains("AgentCapReached"),
            "the fan-out was bounded by the BUDGET, not the lifetime cap: {error_text:?}"
        );
    }

    Ok(())
}

/// UAT-5 (real `args.budget.total`) — a budget sourced from a live `args.budget.total` threaded
/// through the tool call drives the ceiling end-to-end.
///
/// The top-level (freeform) `workflow` tool carries no structured args, so a real `args.budget.total`
/// reaches the engine through the NESTED `workflow(name, args)` path: the depth-0 run calls
/// `workflow('metered', { budget: { total: 250 } })`, and the saved `metered` workflow's subagents
/// meter their OUTPUT tokens against that caller-supplied ceiling. The engine artifact is the nested
/// run's own return (surfaced verbatim in the parent's workflow tool output): ordinals 0/1/2 ran and
/// ordinal 3 threw `BudgetExceeded` at `remaining() == 0` — proving the `args.budget.total` value
/// (not a session-configured baseline) installed and enforced the ceiling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn uat5_nested_args_budget_total_drives_ceiling() -> Result<()> {
    let server = responses::start_mock_server().await;

    let parent_source = r#"export const meta = { name: 'uat5argsparent', description: 'args.budget.total via nested run' };
text(await workflow('metered', { budget: { total: 250 } }));
"#;

    // Each subagent turn reports 100 OUTPUT tokens (the nested budget uses the pure output-weight
    // config), so spend accrues 100/turn against the caller-supplied 250 ceiling.
    let subagent: SubagentScript = Box::new(|ordinal, _is_followup| {
        sse(vec![
            ev_response_created(&format!("resp-sub-{ordinal}")),
            ev_assistant_message(&format!("msg-sub-{ordinal}"), &format!("done-{ordinal}")),
            ev_completed_with_output_tokens(&format!("resp-sub-{ordinal}"), 100),
        ])
    });
    let seen = mount_workflow_router(&server, parent_source, subagent).await;

    let test = workflow_builder(&server.uri())
        .with_config(|config| {
            // No session baseline: the ONLY ceiling is the one carried by args.budget.total.
            config.rollout_budget = None;
            config.multi_agent_v2.max_concurrent_threads_per_session = 32;
            let workflows_dir = config.codex_home.as_path().join("workflows");
            std::fs::create_dir_all(&workflows_dir).expect("create workflows dir");
            std::fs::write(
                workflows_dir.join("metered.workflow.js"),
                "export const meta = { name: 'metered', description: 'nested metered run' };\n\
                 const results = [];\n\
                 for (let i = 0; i < 5; i++) {\n\
                 \x20 try {\n\
                 \x20   await agent('WFUAT_SUB_' + i + ' budgeted work');\n\
                 \x20   results.push(i);\n\
                 \x20 } catch (e) {\n\
                 \x20   results.push({ threwAt: i, error: String(e) });\n\
                 \x20   break;\n\
                 \x20 }\n\
                 }\n\
                 text(JSON.stringify(results));\n",
            )
            .expect("write metered workflow");
        })
        .build(&server)
        .await?;

    test.submit_turn("run the nested args.budget workflow")
        .await?;

    // The nested run's own return, surfaced verbatim in the parent's workflow tool output.
    let results = workflow_return_array(&seen);
    assert_eq!(
        results.len(),
        4,
        "args.budget.total=250 admits ordinals 0/1/2 then throws at 3: {results:?}"
    );
    assert_eq!(results[0], json!(0));
    assert_eq!(results[1], json!(1));
    assert_eq!(results[2], json!(2));
    assert_eq!(
        results[3]["threwAt"],
        json!(3),
        "the caller-supplied ceiling throws at the exact ordinal where remaining() hits 0: {results:?}"
    );
    let error_text = results[3]["error"].as_str().unwrap_or_default();
    assert!(
        error_text.contains("BudgetExceeded"),
        "a real args.budget.total ceiling must throw BudgetExceeded, got: {error_text:?}"
    );

    // The refused ordinal never dispatched; the admitted ones did.
    for ordinal in 0..3 {
        assert!(
            subagent_arrival_index(&seen, ordinal).is_some(),
            "ordinal {ordinal} was admitted under the args.budget ceiling and must have dispatched"
        );
    }
    assert!(
        subagent_arrival_index(&seen, 3).is_none(),
        "ordinal 3 was refused by the args.budget ceiling and must never dispatch"
    );

    Ok(())
}

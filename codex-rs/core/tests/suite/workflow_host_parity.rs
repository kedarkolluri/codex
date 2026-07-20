#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Cross-host parity gate for the saved-only Dynamic Workflow entrypoint.
//!
//! The same bounded fan-out is launched through the model-visible `workflow_run`
//! function under both code-mode hosts. The fixture uses one local OpenAI-compatible
//! router and rotates only secret header values between lanes. Normalized workflow
//! events and the durable journal must remain identical.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use codex_core::config::AgentRoleConfig;
use codex_features::Feature;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::WorkflowEvent;
use codex_workflow_journal::WorkflowRunStatus;
use codex_workflow_journal::storage::WorkflowRunPaths;
use core_test_support::TestTargetOs;
use core_test_support::responses;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::test_codex::test_codex;
use core_test_support::test_target_os;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const WORKFLOW_CALL_ID: &str = "call-workflow-parity";
const CHILD_TOOL_CALL_ID: &str = "call-host-parity-child-tool";
const WORKFLOW_NAME: &str = "host-parity";
const PROVIDER_ID: &str = "parity_router";
const PROVIDER_NAME: &str = "Parity Router";
const ROUTER_HEADER: &str = "x-parity-secret";
const PARENT_MODEL: &str = "gpt-5.5";
const OVERRIDE_MODEL: &str = "gpt-5.4";
const ROLE_MODEL: &str = "gpt-5.4-mini";
const AGENT_MARKER: &str = "HOST_PARITY_AGENT_";

const WORKFLOW_SOURCE: &str = r#"export const meta = {
  name: 'host-parity',
  description: 'bounded cross-host provider parity',
  phases: ['fanout'],
};
phase('fanout');
const results = await parallel([
  () => agent('HOST_PARITY_AGENT_0 inherited defaults', { label: 'inherited' }),
  () => agent('HOST_PARITY_AGENT_1 model effort override', {
    label: 'overridden', model: 'gpt-5.4', effort: 'high'
  }),
  () => agent('HOST_PARITY_AGENT_2 reviewer role', {
    label: 'reviewer', agentType: 'reviewer'
  }),
]);
log('host-parity-complete:' + results.length);
text(JSON.stringify(results));
"#;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HostMode {
    InProcess,
    ProcessOwned,
}

impl HostMode {
    fn secret(self) -> &'static str {
        match self {
            Self::InProcess => "in-process-secret",
            Self::ProcessOwned => "process-owned-secret",
        }
    }
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    body: Value,
    router_secret: Option<String>,
    authorization: Option<String>,
}

#[derive(Clone, Default)]
struct ParityRouter {
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl Respond for ParityRouter {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = request_body_json(request);
        let router_secret = request
            .headers
            .get(ROUTER_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let authorization = request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        self.requests.lock().unwrap().push(RecordedRequest {
            body: body.clone(),
            router_secret,
            authorization,
        });

        if let Some(ordinal) = subagent_ordinal(&body) {
            let delay = Duration::from_millis(50 + ordinal * 300);
            if ordinal == 0 {
                if contains_function_output(&body, CHILD_TOOL_CALL_ID) {
                    return sse_response(sse(vec![
                        ev_response_created("resp-child-0-final"),
                        ev_assistant_message("msg-child-0-final", "child-0"),
                        ev_completed_with_tokens("resp-child-0-final", 60),
                    ]))
                    .set_delay(delay);
                }
                return sse_response(sse(vec![
                    ev_response_created("resp-child-0-tool"),
                    ev_function_call(
                        CHILD_TOOL_CALL_ID,
                        "shell_command",
                        &child_tool_arguments().to_string(),
                    ),
                    ev_completed_with_tokens("resp-child-0-tool", 40),
                ]))
                .set_delay(delay);
            }
            return sse_response(sse(vec![
                ev_response_created(&format!("resp-child-{ordinal}")),
                ev_assistant_message(&format!("msg-child-{ordinal}"), &format!("child-{ordinal}")),
                ev_completed_with_tokens(&format!("resp-child-{ordinal}"), 100 + ordinal as i64),
            ]))
            .set_delay(delay);
        }

        if contains_workflow_output(&body) {
            return sse_response(sse(vec![
                ev_response_created("resp-parent-close"),
                ev_assistant_message("msg-parent-close", "workflow admitted"),
                ev_completed("resp-parent-close"),
            ]));
        }

        let arguments = json!({
            "name": WORKFLOW_NAME,
            "args": { "fixture": "bounded" },
        })
        .to_string();
        sse_response(sse(vec![
            ev_response_created("resp-parent-open"),
            ev_function_call(WORKFLOW_CALL_ID, "workflow_run", &arguments),
            ev_completed("resp-parent-open"),
        ]))
    }
}

#[derive(Debug)]
struct LaneOutcome {
    execution_fingerprint: String,
    counter_traces: BTreeMap<String, AgentCounterTrace>,
    normalized_event_projection: Vec<String>,
    normalized_ordered_events: Vec<String>,
    normalized_journal: Vec<String>,
    normalized_meta: Value,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn saved_fanout_matches_across_in_process_and_process_owned_hosts() -> Result<()> {
    let server = responses::start_mock_server().await;
    let router = ParityRouter::default();
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(router.clone())
        .mount(&server)
        .await;

    let in_process = run_lane(&server, &router, HostMode::InProcess).await?;
    let process_owned = run_lane(&server, &router, HostMode::ProcessOwned).await?;

    assert_eq!(
        process_owned.execution_fingerprint, in_process.execution_fingerprint,
        "rotating bearer/header values and host mode must not change provider identity",
    );
    assert_eq!(process_owned.normalized_meta, in_process.normalized_meta);
    assert_eq!(
        process_owned.counter_traces, in_process.counter_traces,
        "live counter transitions and terminal values must match across hosts",
    );
    assert_eq!(
        process_owned.normalized_event_projection, in_process.normalized_event_projection,
        "normalized workflow topology and terminal counters must match",
    );
    assert_eq!(
        process_owned.normalized_ordered_events, in_process.normalized_ordered_events,
        "normalized workflow lifecycle event ordering must match",
    );
    assert_eq!(
        process_owned.normalized_journal, in_process.normalized_journal,
        "normalized durable agent outcomes must match",
    );

    Ok(())
}

async fn run_lane(
    server: &MockServer,
    router: &ParityRouter,
    host_mode: HostMode,
) -> Result<LaneOutcome> {
    let secret = host_mode.secret();
    let request_start = router.requests.lock().unwrap().len();
    let provider = parity_provider(&server.uri(), secret);
    let provider_for_config = provider.clone();
    let server_uri = server.uri();
    let mut builder = test_codex()
        .with_model(PARENT_MODEL)
        .with_pre_build_hook(move |codex_home| {
            write_saved_fixture(codex_home, &server_uri, secret);
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
            config.model_provider_id = PROVIDER_ID.to_string();
            config.model_provider = provider_for_config;
            config.model_reasoning_effort = Some(ReasoningEffort::Medium);
            config.multi_agent_v2.max_concurrent_threads_per_session = 16;
            config.agent_roles.insert(
                "reviewer".to_string(),
                AgentRoleConfig {
                    description: Some("Parity reviewer role".to_string()),
                    config_file: Some(config.codex_home.as_path().join("reviewer.toml")),
                    nickname_candidates: None,
                },
            );
            match host_mode {
                HostMode::InProcess => config
                    .features
                    .disable(Feature::CodeModeHost)
                    .expect("disable process host"),
                HostMode::ProcessOwned => config
                    .features
                    .enable(Feature::CodeModeHost)
                    .expect("enable process host"),
            }
        });
    if host_mode == HostMode::ProcessOwned {
        let host_program = codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")
            .context("process-owned parity lane requires the real code-mode host binary")?;
        builder = builder.with_code_mode_host_program(host_program);
    }

    let test = builder.build_with_auto_env(server).await?;
    assert_eq!(
        test.codex.config_snapshot().await.model_provider_id,
        PROVIDER_ID
    );
    let mut workflow_events = test.codex.subscribe_events();
    test.submit_turn("start the saved parity workflow").await?;
    let (run_id, events) = collect_workflow_events(&mut workflow_events).await?;
    assert_agent_lifecycle_partial_order(&events);
    let counter_traces = assert_live_counter_contract(&events, host_mode);

    let child_ids = bound_child_ids(&events)?;
    assert_eq!(child_ids.len(), 3, "one persistent child per fan-out item");
    let mut child_configs = BTreeMap::new();
    for child_id in child_ids {
        let child = test.thread_manager.get_thread(child_id).await?;
        let snapshot = child.config_snapshot().await;
        assert_eq!(
            snapshot.model_provider_id, PROVIDER_ID,
            "model/effort/role overrides must retain the configured provider",
        );
        let role = match snapshot.session_source {
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_role, .. }) => agent_role,
            other => panic!("workflow child has unexpected session source: {other:?}"),
        };
        child_configs.insert(
            snapshot.model.clone(),
            (snapshot.reasoning_effort.clone(), role),
        );
    }
    assert_eq!(
        child_configs.get(PARENT_MODEL),
        Some(&(Some(ReasoningEffort::Medium), None)),
        "an unqualified agent inherits parent model/effort and the default role",
    );
    assert_eq!(
        child_configs.get(OVERRIDE_MODEL),
        Some(&(Some(ReasoningEffort::High), None)),
        "the per-agent override changes model and effort only",
    );
    assert_eq!(
        child_configs.get(ROLE_MODEL),
        Some(&(Some(ReasoningEffort::Low), Some("reviewer".to_string()))),
        "the reviewer role applies its locked model/effort and remains visible in child metadata",
    );

    assert_router_requests(
        &router.requests.lock().unwrap()[request_start..],
        secret,
        host_mode,
    );

    let paths = WorkflowRunPaths::new(test.codex_home_path(), &run_id);
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(paths.meta())?)?;
    assert_eq!(meta["status"], json!(WorkflowRunStatus::Completed));
    let execution_fingerprint = meta["execution_fingerprint"]
        .as_str()
        .context("workflow meta must carry an execution fingerprint")?
        .to_string();
    let normalized_meta = normalize_meta(meta);
    let normalized_journal = normalize_journal(&paths.journal())?;

    test.codex.shutdown_and_wait().await?;
    Ok(LaneOutcome {
        execution_fingerprint,
        counter_traces,
        normalized_event_projection: normalize_event_projection(&events),
        normalized_ordered_events: normalize_ordered_events(&events),
        normalized_journal,
        normalized_meta,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentCounterSample {
    token_usage: TokenUsage,
    tool_call_count: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AgentCounterTrace {
    updates: Vec<AgentCounterSample>,
    terminal: AgentCounterSample,
    status: AgentStatus,
    returned_null: bool,
}

fn assert_live_counter_contract(
    events: &[WorkflowEvent],
    host_mode: HostMode,
) -> BTreeMap<String, AgentCounterTrace> {
    let labels = events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::AgentBegin(event) => Some((event.node_id, event.label.clone())),
            _ => None,
        })
        .collect::<BTreeMap<_, _>>();
    let mut traces = BTreeMap::<String, AgentCounterTrace>::new();

    for (node_id, label) in &labels {
        let mut updates = events
            .iter()
            .filter_map(|event| match event {
                WorkflowEvent::AgentUpdated(event) if event.node_id == *node_id => {
                    Some(AgentCounterSample {
                        token_usage: event.token_usage.clone(),
                        tool_call_count: event.tool_call_count,
                    })
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            !updates.is_empty(),
            "{host_mode:?} agent {label} must publish at least one live counter update",
        );

        let mut previous = AgentCounterSample {
            token_usage: TokenUsage::default(),
            tool_call_count: 0,
        };
        let mut strict_growth = 0;
        for update in &updates {
            assert_counter_monotonic(&previous, update, host_mode, label);
            if update != &previous {
                strict_growth += 1;
            }
            previous = update.clone();
        }
        assert!(
            strict_growth > 0,
            "{host_mode:?} agent {label} counter stream must grow from zero",
        );

        // Duplicate redraws are not semantic counter transitions. Removing only adjacent
        // duplicates keeps the exact live growth sequence comparable across IPC boundaries.
        updates.dedup();

        let ends = events
            .iter()
            .filter_map(|event| match event {
                WorkflowEvent::AgentEnd(event) if event.node_id == *node_id => Some(event),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            ends.len(),
            1,
            "{host_mode:?} agent {label} must have exactly one terminal event",
        );
        let end = ends[0];
        let terminal = AgentCounterSample {
            token_usage: end.token_usage.clone(),
            tool_call_count: end.tool_call_count,
        };
        assert_eq!(
            updates.last(),
            Some(&terminal),
            "{host_mode:?} agent {label} final update must equal its terminal counters",
        );
        assert_eq!(end.status, AgentStatus::Completed(None));
        assert!(!end.returned_null);

        traces.insert(
            label.clone(),
            AgentCounterTrace {
                updates,
                terminal,
                status: end.status.clone(),
                returned_null: end.returned_null,
            },
        );
    }

    let expected_terminal = BTreeMap::from([
        ("inherited".to_string(), expected_counter_sample(100, 1)),
        ("overridden".to_string(), expected_counter_sample(101, 0)),
        ("reviewer".to_string(), expected_counter_sample(102, 0)),
    ]);
    assert_eq!(
        traces
            .iter()
            .map(|(label, trace)| (label.clone(), trace.terminal.clone()))
            .collect::<BTreeMap<_, _>>(),
        expected_terminal,
        "{host_mode:?} terminal counters must equal the deterministic Responses usage",
    );

    let inherited = &traces["inherited"];
    assert!(
        inherited.updates.contains(&expected_counter_sample(0, 1)),
        "{host_mode:?} must expose the real child tool call before terminal token usage",
    );
    assert!(
        inherited.updates.contains(&expected_counter_sample(40, 1)),
        "{host_mode:?} must expose the first model round-trip's fixed token usage",
    );
    assert!(
        inherited
            .updates
            .iter()
            .any(|update| update != &inherited.terminal),
        "{host_mode:?} live-counter coverage must observe a pre-terminal value",
    );

    traces
}

fn assert_counter_monotonic(
    previous: &AgentCounterSample,
    current: &AgentCounterSample,
    host_mode: HostMode,
    label: &str,
) {
    assert!(
        current.token_usage.input_tokens >= previous.token_usage.input_tokens
            && current.token_usage.cached_input_tokens >= previous.token_usage.cached_input_tokens
            && current.token_usage.output_tokens >= previous.token_usage.output_tokens
            && current.token_usage.reasoning_output_tokens
                >= previous.token_usage.reasoning_output_tokens
            && current.token_usage.total_tokens >= previous.token_usage.total_tokens
            && current.tool_call_count >= previous.tool_call_count,
        "{host_mode:?} agent {label} counter update regressed: {previous:?} -> {current:?}",
    );
}

fn expected_counter_sample(input_tokens: i64, tool_call_count: u64) -> AgentCounterSample {
    AgentCounterSample {
        token_usage: TokenUsage {
            input_tokens,
            cached_input_tokens: 0,
            output_tokens: 0,
            reasoning_output_tokens: 0,
            total_tokens: input_tokens,
        },
        tool_call_count,
    }
}

fn parity_provider(server_uri: &str, secret: &str) -> ModelProviderInfo {
    let mut provider = codex_model_provider_info::built_in_model_providers(None)["openai"].clone();
    provider.name = PROVIDER_NAME.to_string();
    provider.base_url = Some(format!("{server_uri}/v1"));
    provider.env_key = None;
    provider.experimental_bearer_token = Some(format!("bearer-{secret}"));
    provider.requires_openai_auth = false;
    provider.supports_websockets = false;
    provider.http_headers = Some(HashMap::from([(
        ROUTER_HEADER.to_string(),
        secret.to_string(),
    )]));
    provider
}

fn write_saved_fixture(codex_home: &std::path::Path, server_uri: &str, secret: &str) {
    let workflows = codex_home.join("workflows");
    std::fs::create_dir_all(&workflows).expect("create saved workflow root");
    std::fs::write(workflows.join("host-parity.workflow.js"), WORKFLOW_SOURCE)
        .expect("write saved workflow");
    std::fs::write(
        codex_home.join("reviewer.toml"),
        format!("model = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"low\"\n"),
    )
    .expect("write reviewer role");
    std::fs::write(
        codex_home.join("config.toml"),
        format!(
            "model_provider = \"{PROVIDER_ID}\"\n\
             [model_providers.{PROVIDER_ID}]\n\
             name = \"{PROVIDER_NAME}\"\n\
             base_url = \"{server_uri}/v1\"\n\
             wire_api = \"responses\"\n\
             experimental_bearer_token = \"bearer-{secret}\"\n\
             requires_openai_auth = false\n\
             supports_websockets = false\n\
             [model_providers.{PROVIDER_ID}.http_headers]\n\
             {ROUTER_HEADER} = \"{secret}\"\n"
        ),
    )
    .expect("write persisted parity provider");
}

async fn collect_workflow_events(
    receiver: &mut tokio::sync::broadcast::Receiver<codex_protocol::protocol::Event>,
) -> Result<(String, Vec<WorkflowEvent>)> {
    let mut run_id = None;
    let mut events = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), receiver.recv())
            .await
            .context("timed out waiting for workflow completion")??;
        let EventMsg::Workflow(workflow_event) = event.msg else {
            continue;
        };
        let event_run_id = workflow_event_run_id(&workflow_event);
        match &run_id {
            Some(expected) if expected != event_run_id => continue,
            None => run_id = Some(event_run_id.to_string()),
            Some(_) => {}
        }
        let terminal = matches!(workflow_event, WorkflowEvent::RunEnd(_));
        events.push(workflow_event);
        if terminal {
            return Ok((run_id.expect("run id observed"), events));
        }
    }
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

fn bound_child_ids(events: &[WorkflowEvent]) -> Result<Vec<ThreadId>> {
    events
        .iter()
        .filter_map(|event| match event {
            WorkflowEvent::AgentBound(bound) => Some(bound.child_thread_id.as_str()),
            _ => None,
        })
        .map(ThreadId::from_string)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[derive(Debug, Default)]
struct AgentLifecyclePositions {
    begin: Vec<usize>,
    bound: Vec<usize>,
    end: Vec<usize>,
}

fn assert_agent_lifecycle_partial_order(events: &[WorkflowEvent]) {
    let mut by_node = BTreeMap::<u64, AgentLifecyclePositions>::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            WorkflowEvent::AgentBegin(event) => {
                by_node.entry(event.node_id).or_default().begin.push(index)
            }
            WorkflowEvent::AgentBound(event) => {
                by_node.entry(event.node_id).or_default().bound.push(index)
            }
            WorkflowEvent::AgentEnd(event) => {
                by_node.entry(event.node_id).or_default().end.push(index)
            }
            _ => {}
        }
    }
    assert_eq!(by_node.len(), 3, "one lifecycle per fan-out node");
    for (node_id, positions) in by_node {
        assert_eq!(
            positions.begin.len(),
            1,
            "node {node_id} begins exactly once"
        );
        assert_eq!(
            positions.bound.len(),
            1,
            "node {node_id} binds exactly once"
        );
        assert_eq!(positions.end.len(), 1, "node {node_id} ends exactly once");
        assert!(
            positions.begin[0] < positions.bound[0] && positions.bound[0] < positions.end[0],
            "node {node_id} must emit Begin then Bound then End: {positions:?}",
        );
    }
}

fn normalize_event_projection(events: &[WorkflowEvent]) -> Vec<String> {
    let mut by_key = BTreeMap::new();
    for event in events {
        let value = normalize_event(event);
        let event_name = value["event"].as_str().unwrap_or_default().to_string();
        let ordinal = value
            .get("node_id")
            .or_else(|| value.get("group_id"))
            .or_else(|| value.get("phase_index"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let key = format!("{event_name}:{ordinal:020}");
        // AgentUpdated is a live redraw stream; parity compares its final projection.
        // All other keys are unique in this fixture.
        by_key.insert(key, serde_json::to_string(&value).expect("encode event"));
    }
    by_key.into_values().collect()
}

fn normalize_ordered_events(events: &[WorkflowEvent]) -> Vec<String> {
    let mut ordered = Vec::new();
    let mut concurrent_admissions = Vec::new();
    for event in events {
        // AgentUpdated is a transport redraw stream whose final projection is compared above.
        if matches!(event, WorkflowEvent::AgentUpdated(_)) {
            continue;
        }
        let normalized = normalize_event(event);
        let encoded = serde_json::to_string(&normalized).expect("encode event");
        match event {
            // Fan-out admission is intentionally concurrent: cross-node Begin/Bound interleaving
            // is not a total-order contract. The assertion above proves each node's Begin < Bound
            // < End partial order. Canonicalize only this admission prefix; after the first
            // AgentEnd every lifecycle event remains in raw observed order for the host comparison.
            WorkflowEvent::AgentBegin(event) => {
                concurrent_admissions.push((event.node_id, 0, encoded));
            }
            WorkflowEvent::AgentBound(event) => {
                concurrent_admissions.push((event.node_id, 1, encoded));
            }
            _ => {
                if !concurrent_admissions.is_empty() {
                    concurrent_admissions.sort_by_key(|(node_id, stage, _)| (*node_id, *stage));
                    ordered.extend(concurrent_admissions.drain(..).map(|(_, _, event)| event));
                }
                ordered.push(encoded);
            }
        }
    }
    ordered.extend(concurrent_admissions.drain(..).map(|(_, _, event)| event));
    ordered
}

fn normalize_event(event: &WorkflowEvent) -> Value {
    let mut value = serde_json::to_value(event).expect("serialize workflow event");
    if let Some(object) = value.as_object_mut() {
        object.insert("run_id".to_string(), json!("<run>"));
        object.remove("duration_ms");
        if object.contains_key("child_thread_id") {
            object.insert("child_thread_id".to_string(), json!("<thread>"));
        }
    }
    value
}

fn normalize_journal(path: &std::path::Path) -> Result<Vec<String>> {
    let contents = std::fs::read_to_string(path)?;
    let mut normalized = Vec::new();
    for line in contents.lines().skip(1).filter(|line| !line.is_empty()) {
        let mut value: Value = serde_json::from_str(line)?;
        let object = value
            .as_object_mut()
            .context("journal line must be an object")?;
        object.remove("timestamp");
        object.remove("duration_ms");
        if let Some(progress) = object.get_mut("progress").and_then(Value::as_object_mut) {
            progress.remove("duration_ms");
        }
        if object.contains_key("child_thread_id") {
            object.insert("child_thread_id".to_string(), json!("<thread>"));
        }
        if object.contains_key("rollout_path") {
            object.insert("rollout_path".to_string(), json!("<absolute-rollout>"));
        }
        if object.contains_key("completion_seq") {
            object.insert("completion_seq".to_string(), json!("<completion-order>"));
        }
        normalized.push(serde_json::to_string(&value)?);
    }
    normalized.sort();
    Ok(normalized)
}

fn normalize_meta(mut meta: Value) -> Value {
    let object = meta
        .as_object_mut()
        .expect("workflow meta must be an object");
    object.insert("run_id".to_string(), json!("<run>"));
    object.remove("created_at");
    object.remove("execution_fingerprint");
    object.remove("owner_thread_id");
    meta
}

fn assert_router_requests(requests: &[RecordedRequest], secret: &str, host_mode: HostMode) {
    assert!(
        requests.len() >= 6,
        "{host_mode:?} should issue two parent requests, three child openings, and one child tool follow-up"
    );
    for request in requests {
        assert_eq!(request.router_secret.as_deref(), Some(secret));
        assert_eq!(
            request.authorization.as_deref(),
            Some(format!("Bearer bearer-{secret}").as_str()),
        );
    }

    let children = requests
        .iter()
        .filter_map(|request| {
            subagent_ordinal(&request.body).map(|ordinal| (ordinal, &request.body))
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(children.len(), 3);
    assert_request_identity(children[&0], PARENT_MODEL, "medium");
    assert_request_identity(children[&1], OVERRIDE_MODEL, "high");
    assert_request_identity(children[&2], ROLE_MODEL, "low");

    let child_tool_followups = requests
        .iter()
        .filter(|request| contains_function_output(&request.body, CHILD_TOOL_CALL_ID))
        .collect::<Vec<_>>();
    assert_eq!(
        child_tool_followups.len(),
        1,
        "{host_mode:?} must execute exactly one real child tool round-trip",
    );
    assert!(
        serde_json::to_string(&child_tool_followups[0].body)
            .expect("encode child tool follow-up")
            .contains("HOST_PARITY_TOOL_OK"),
        "{host_mode:?} child tool output must carry the command's success marker",
    );
}

fn assert_request_identity(body: &Value, model: &str, effort: &str) {
    assert_eq!(body["model"], json!(model));
    assert_eq!(body.pointer("/reasoning/effort"), Some(&json!(effort)));
}

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

fn subagent_ordinal(body: &Value) -> Option<u64> {
    body["input"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .filter_map(|item| item.get("content").and_then(Value::as_array))
        .flatten()
        .filter_map(|span| span.get("text").and_then(Value::as_str))
        .find_map(|text| {
            text.split(AGENT_MARKER)
                .nth(1)
                .and_then(|rest| rest.split(|ch: char| !ch.is_ascii_digit()).next())
                .and_then(|digits| digits.parse().ok())
        })
}

fn contains_workflow_output(body: &Value) -> bool {
    body["input"].as_array().into_iter().flatten().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(WORKFLOW_CALL_ID)
    })
}

fn contains_function_output(body: &Value, call_id: &str) -> bool {
    body["input"].as_array().into_iter().flatten().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
    })
}

fn child_tool_arguments() -> Value {
    let command = match test_target_os() {
        TestTargetOs::Linux | TestTargetOs::MacOs => "printf HOST_PARITY_TOOL_OK",
        TestTargetOs::Windows => "Write-Output HOST_PARITY_TOOL_OK",
    };
    json!({
        "command": command,
        "login": false,
        "timeout_ms": 10_000,
    })
}

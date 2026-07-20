#![allow(clippy::expect_used, clippy::unwrap_used)]
//! Real-stack coverage for dispatch-broker ownership across overlapping workflow cells.

use std::collections::BTreeMap;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::WorkflowEvent;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use wiremock::Match;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const WORKFLOW_NAME: &str = "broker-overlap";
const PARENT_A: &str = "BROKER_PARENT_A";
const PARENT_B: &str = "BROKER_PARENT_B";
const CHILD_A: &str = "BROKER_CHILD_A";
const CHILD_B: &str = "BROKER_CHILD_B";
const CALL_A: &str = "call-broker-workflow-a";
const CALL_B: &str = "call-broker-workflow-b";
const MODEL_A: &str = "gpt-5.4";
const MODEL_B: &str = "gpt-5.4-mini";
const MAX_LATCH_ROUNDS: usize = 128;

const WORKFLOW_SOURCE: &str = r#"// @exec: {"yield_time_ms": 1}
export const meta = { name: 'broker-overlap', description: 'broker ownership gate' };
const lane = args.lane;
phase('lane-' + lane);
const result = await agent('BROKER_CHILD_' + lane, {
  label: 'lane-' + lane,
  model: args.model,
  effort: args.effort,
});
log('BROKER_RESULT:' + lane + ':' + result);
text(JSON.stringify({ lane, result }));
"#;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum Lane {
    A,
    B,
}

impl Lane {
    fn id(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
        }
    }

    fn child_marker(self) -> &'static str {
        match self {
            Self::A => CHILD_A,
            Self::B => CHILD_B,
        }
    }

    fn model(self) -> &'static str {
        match self {
            Self::A => MODEL_A,
            Self::B => MODEL_B,
        }
    }

    fn effort(self) -> ReasoningEffort {
        match self {
            Self::A => ReasoningEffort::High,
            Self::B => ReasoningEffort::Low,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum HostMode {
    InProcess,
    ProcessOwned,
}

#[derive(Clone, Copy)]
enum ParentRequestKind {
    Open(Lane),
    Close(Lane),
}

struct ParentRequestMatcher(ParentRequestKind);

impl Match for ParentRequestMatcher {
    fn matches(&self, request: &wiremock::Request) -> bool {
        let body = request_body_json(request);
        match self.0 {
            ParentRequestKind::Open(Lane::A) => {
                contains_user_text(&body, PARENT_A) && !contains_call_output(&body, CALL_A)
            }
            ParentRequestKind::Open(Lane::B) => {
                contains_user_text(&body, PARENT_B) && !contains_call_output(&body, CALL_B)
            }
            ParentRequestKind::Close(Lane::A) => contains_call_output(&body, CALL_A),
            ParentRequestKind::Close(Lane::B) => contains_call_output(&body, CALL_B),
        }
    }
}

struct ChildRequestMatcher;

impl Match for ChildRequestMatcher {
    fn matches(&self, request: &wiremock::Request) -> bool {
        child_lane(&request_body_json(request)).is_some()
    }
}

#[derive(Clone, Debug)]
struct ChildRequest {
    lane: Lane,
    model: String,
    effort: String,
    served_final: bool,
}

struct ChildRouter {
    seen: Arc<Mutex<Vec<ChildRequest>>>,
}

impl Respond for ChildRouter {
    fn respond(&self, request: &wiremock::Request) -> ResponseTemplate {
        let body = request_body_json(request);
        let lane = child_lane(&body).expect("child matcher admitted only child requests");
        let mut seen = self.seen.lock().unwrap();
        let b_seen = seen.iter().any(|request| request.lane == Lane::B);
        let round = seen.iter().filter(|request| request.lane == lane).count() + 1;
        let served_final = lane == Lane::B || b_seen || round >= MAX_LATCH_ROUNDS;
        seen.push(ChildRequest {
            lane,
            model: body["model"].as_str().unwrap_or_default().to_string(),
            effort: body["reasoning"]["effort"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            served_final,
        });
        drop(seen);

        if served_final {
            return sse_response(sse(vec![
                ev_response_created(&format!("resp-child-{}-final", lane.id())),
                ev_assistant_message(
                    &format!("msg-child-{}-final", lane.id()),
                    &format!("result-{}", lane.id()),
                ),
                ev_completed(&format!("resp-child-{}-final", lane.id())),
            ]));
        }

        let arguments = json!({
            "plan": [{ "step": format!("hold-A-{round}"), "status": "in_progress" }],
        })
        .to_string();
        sse_response(sse(vec![
            ev_response_created(&format!("resp-child-A-latch-{round}")),
            ev_function_call(
                &format!("call-child-A-latch-{round}"),
                "update_plan",
                &arguments,
            ),
            ev_completed(&format!("resp-child-A-latch-{round}")),
        ]))
    }
}

struct ParentMocks {
    open_a: ResponseMock,
    close_a: ResponseMock,
    open_b: ResponseMock,
    close_b: ResponseMock,
}

#[derive(Default)]
struct RunObservation {
    run_id: String,
    model: String,
    effort: Option<ReasoningEffort>,
    child_thread_id: String,
    log: String,
    agent_status: Option<AgentStatus>,
    run_status: Option<AgentStatus>,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn yielded_workflow_cells_keep_real_turn_hosts_without_cross_talk() -> Result<()> {
    for host_mode in [HostMode::InProcess, HostMode::ProcessOwned] {
        run_lane(host_mode).await?;
    }
    Ok(())
}

async fn run_lane(host_mode: HostMode) -> Result<()> {
    let server = responses::start_mock_server().await;
    let parent_mocks = mount_parent_mocks(&server).await;
    let child_requests = Arc::new(Mutex::new(Vec::new()));
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(ChildRequestMatcher)
        .respond_with(ChildRouter {
            seen: Arc::clone(&child_requests),
        })
        .mount(&server)
        .await;

    let test = workflow_builder(&server.uri(), host_mode)
        .build_with_auto_env(&server)
        .await?;
    let parent_thread_id = test.session_configured.thread_id;
    let mut workflow_events = test.codex.subscribe_events();

    test.submit_turn(PARENT_A).await?;
    test.submit_turn(PARENT_B).await?;

    let observations = collect_two_runs(&mut workflow_events).await?;
    assert_lane_observation(&test, parent_thread_id, Lane::A, &observations).await?;
    assert_lane_observation(&test, parent_thread_id, Lane::B, &observations).await?;
    assert_ne!(
        observations[&Lane::A].run_id,
        observations[&Lane::B].run_id,
        "overlapping cells must retain distinct durable runs",
    );
    assert_ne!(
        observations[&Lane::A].child_thread_id,
        observations[&Lane::B].child_thread_id,
        "overlapping broker routes must bind distinct child threads",
    );

    let child_requests_at_terminal = {
        let requests = child_requests.lock().unwrap();
        let first_b = requests
            .iter()
            .position(|request| request.lane == Lane::B)
            .context("lane B child reached the fixture router")?;
        assert!(
            requests[..first_b]
                .iter()
                .any(|request| request.lane == Lane::A && !request.served_final),
            "lane A must be yielded before the replacement turn starts lane B",
        );
        assert!(
            requests[first_b + 1..]
                .iter()
                .any(|request| request.lane == Lane::A && request.served_final),
            "lane A must resume only after lane B proves the replacement host is live",
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.lane == Lane::A)
                .all(|request| request.model == MODEL_A && request.effort == "high"),
            "lane A requests crossed into lane B's captured model context: {requests:?}",
        );
        assert!(
            requests
                .iter()
                .filter(|request| request.lane == Lane::B)
                .all(|request| request.model == MODEL_B && request.effort == "low"),
            "lane B requests crossed into lane A's captured model context: {requests:?}",
        );
        requests.len()
    };

    assert_parent_mocks(&parent_mocks, &observations);
    test.codex.submit(Op::Shutdown).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    test.codex.wait_until_terminated().await;
    assert_eq!(
        child_requests.lock().unwrap().len(),
        child_requests_at_terminal,
        "shutdown exposed a detached broker callback after both runs were terminal",
    );
    Ok(())
}

async fn mount_parent_mocks(server: &MockServer) -> ParentMocks {
    ParentMocks {
        open_a: responses::mount_sse_once_match(
            server,
            ParentRequestMatcher(ParentRequestKind::Open(Lane::A)),
            workflow_call_sse(Lane::A),
        )
        .await,
        close_a: responses::mount_sse_once_match(
            server,
            ParentRequestMatcher(ParentRequestKind::Close(Lane::A)),
            parent_close_sse(Lane::A),
        )
        .await,
        open_b: responses::mount_sse_once_match(
            server,
            ParentRequestMatcher(ParentRequestKind::Open(Lane::B)),
            workflow_call_sse(Lane::B),
        )
        .await,
        close_b: responses::mount_sse_once_match(
            server,
            ParentRequestMatcher(ParentRequestKind::Close(Lane::B)),
            parent_close_sse(Lane::B),
        )
        .await,
    }
}

fn workflow_call_sse(lane: Lane) -> String {
    let call_id = match lane {
        Lane::A => CALL_A,
        Lane::B => CALL_B,
    };
    let arguments = json!({
        "name": WORKFLOW_NAME,
        "args": {
            "lane": lane.id(),
            "model": lane.model(),
            "effort": lane.effort().to_string(),
        },
    })
    .to_string();
    sse(vec![
        ev_response_created(&format!("resp-parent-{}-open", lane.id())),
        ev_function_call(call_id, "workflow_run", &arguments),
        ev_completed(&format!("resp-parent-{}-open", lane.id())),
    ])
}

fn parent_close_sse(lane: Lane) -> String {
    sse(vec![
        ev_response_created(&format!("resp-parent-{}-close", lane.id())),
        ev_assistant_message(
            &format!("msg-parent-{}-close", lane.id()),
            &format!("admitted-{}", lane.id()),
        ),
        ev_completed(&format!("resp-parent-{}-close", lane.id())),
    ])
}

fn workflow_builder(server_uri: &str, host_mode: HostMode) -> TestCodexBuilder {
    let server_uri = server_uri.to_string();
    let builder = test_codex()
        .with_model("gpt-5.5")
        .with_pre_build_hook(move |home| {
            std::fs::write(
                home.join("config.toml"),
                format!("openai_base_url = \"{server_uri}/v1\"\n"),
            )
            .expect("write hermetic provider config");
            let workflows = home.join("workflows");
            std::fs::create_dir_all(&workflows).expect("create workflow registry");
            std::fs::write(
                workflows.join("broker-overlap.workflow.js"),
                WORKFLOW_SOURCE,
            )
            .expect("write saved workflow");
        })
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Workflow)
                .expect("enable workflow feature");
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
    match host_mode {
        HostMode::InProcess => builder,
        HostMode::ProcessOwned => builder.with_code_mode_host_program(
            codex_utils_cargo_bin::cargo_bin("codex-code-mode-host")
                .expect("resolve real process code-mode host"),
        ),
    }
}

async fn collect_two_runs(
    receiver: &mut tokio::sync::broadcast::Receiver<Event>,
) -> Result<BTreeMap<Lane, RunObservation>> {
    let mut run_to_lane = BTreeMap::<String, Lane>::new();
    let mut observations = BTreeMap::<Lane, RunObservation>::new();
    let mut terminal_count = 0;
    while terminal_count < 2 {
        let event = receiver
            .recv()
            .await
            .context("workflow event stream closed")?;
        let EventMsg::Workflow(event) = event.msg else {
            continue;
        };
        match event {
            WorkflowEvent::AgentBegin(event) => {
                let lane = match event.label.as_str() {
                    "lane-A" => Lane::A,
                    "lane-B" => Lane::B,
                    other => anyhow::bail!("unexpected workflow agent label {other}"),
                };
                run_to_lane.insert(event.run_id.clone(), lane);
                let observation = observations.entry(lane).or_default();
                observation.run_id = event.run_id;
                observation.model = event.model;
                observation.effort = Some(event.effort);
            }
            WorkflowEvent::AgentBound(event) => {
                if let Some(lane) = run_to_lane.get(&event.run_id) {
                    observations.entry(*lane).or_default().child_thread_id = event.child_thread_id;
                }
            }
            WorkflowEvent::AgentEnd(event) => {
                if let Some(lane) = run_to_lane.get(&event.run_id) {
                    observations.entry(*lane).or_default().agent_status = Some(event.status);
                }
            }
            WorkflowEvent::Log(event) => {
                if let Some(lane) = run_to_lane.get(&event.run_id) {
                    observations.entry(*lane).or_default().log = event.message;
                }
            }
            WorkflowEvent::RunEnd(event) => {
                terminal_count += 1;
                if let Some(lane) = run_to_lane.get(&event.run_id) {
                    observations.entry(*lane).or_default().run_status = Some(event.status);
                }
            }
            WorkflowEvent::RunBegin(_)
            | WorkflowEvent::PhaseBegin(_)
            | WorkflowEvent::PhaseEnd(_)
            | WorkflowEvent::GroupBegin(_)
            | WorkflowEvent::GroupEnd(_)
            | WorkflowEvent::AgentUpdated(_) => {}
        }
    }
    Ok(observations)
}

async fn assert_lane_observation(
    test: &TestCodex,
    parent_thread_id: ThreadId,
    lane: Lane,
    observations: &BTreeMap<Lane, RunObservation>,
) -> Result<()> {
    let observation = observations
        .get(&lane)
        .context("missing lane observation")?;
    assert_eq!(observation.model, lane.model());
    assert_eq!(observation.effort, Some(lane.effort()));
    assert_eq!(
        observation.log,
        format!("BROKER_RESULT:{}:result-{}", lane.id(), lane.id()),
    );
    assert_eq!(observation.agent_status, Some(AgentStatus::Completed(None)));
    assert_eq!(observation.run_status, Some(AgentStatus::Completed(None)));

    let child_id = ThreadId::from_string(&observation.child_thread_id)?;
    let child = test.thread_manager.get_thread(child_id).await?;
    let config = child.config_snapshot().await;
    assert_eq!(config.model, lane.model());
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: actual_parent,
        ..
    }) = config.session_source
    else {
        anyhow::bail!("workflow child has non-spawn session source")
    };
    assert_eq!(actual_parent, parent_thread_id);
    Ok(())
}

fn assert_parent_mocks(mocks: &ParentMocks, observations: &BTreeMap<Lane, RunObservation>) {
    for (mock, marker, call_id) in [
        (&mocks.open_a, PARENT_A, CALL_A),
        (&mocks.open_b, PARENT_B, CALL_B),
    ] {
        let matching_requests = mock
            .requests()
            .into_iter()
            .filter(|request| {
                request.body_contains_text(marker)
                    && request.function_call_output_text(call_id).is_none()
            })
            .count();
        assert_eq!(
            matching_requests, 1,
            "expected one parent open for {marker}"
        );
    }
    for (lane, mock, call_id) in [
        (Lane::A, &mocks.close_a, CALL_A),
        (Lane::B, &mocks.close_b, CALL_B),
    ] {
        let output = mock
            .function_call_output_text(call_id)
            .expect("workflow admission output");
        assert!(
            output.contains(&observations[&lane].run_id),
            "parent admission output must carry lane {} run linkage: {output}",
            lane.id(),
        );
    }
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

fn user_texts(body: &Value) -> impl Iterator<Item = &str> {
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
}

fn contains_user_text(body: &Value, needle: &str) -> bool {
    user_texts(body).any(|text| text.contains(needle))
}

fn child_lane(body: &Value) -> Option<Lane> {
    [Lane::A, Lane::B]
        .into_iter()
        .find(|lane| contains_user_text(body, lane.child_marker()))
}

fn contains_call_output(body: &Value, call_id: &str) -> bool {
    body["input"].as_array().into_iter().flatten().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("function_call_output")
            && item.get("call_id").and_then(Value::as_str) == Some(call_id)
    })
}

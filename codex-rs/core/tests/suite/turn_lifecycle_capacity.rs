use anyhow::Result;
use codex_core::CodexThread;
use codex_core::StartThreadOptions;
use codex_core::ThreadManager;
use codex_core::TryStartTurnIfIdleRejectionReason;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_features::Feature;
use codex_protocol::AgentPath;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::sse;
use core_test_support::responses::sse_completed;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_metadata_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);
const CAPACITY_FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);
const WORKER_CREATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct PauseCapacityTurnStarts {
    armed: AtomicBool,
    calls: AtomicUsize,
    paused: AtomicUsize,
    paused_changed: Notify,
    release: Notify,
}

impl PauseCapacityTurnStarts {
    fn release_one(&self) {
        self.release.notify_one();
    }
}

impl codex_extension_api::TurnLifecycleContributor for PauseCapacityTurnStarts {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
            if self.armed.load(Ordering::SeqCst) {
                let paused = self.paused.fetch_add(/*val*/ 1, Ordering::SeqCst) + 1;
                if paused == 2 {
                    self.armed.store(false, Ordering::SeqCst);
                }
                self.paused_changed.notify_waiters();
                self.release.notified().await;
            }
        })
    }
}

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    serde_json::from_slice::<Value>(&request.body).is_ok_and(|body| body.to_string().contains(text))
}

fn request_starts_trigger_turn(
    request: &wiremock::Request,
    trigger_content: &str,
    quiescence_probe: &str,
) -> bool {
    let Ok(body) = serde_json::from_slice::<Value>(&request.body) else {
        return false;
    };
    let Some(input) = body["input"].as_array() else {
        return false;
    };
    let trigger_position = input
        .iter()
        .rposition(|item| item.to_string().contains(trigger_content));
    let quiescence_position = input
        .iter()
        .rposition(|item| item.to_string().contains(quiescence_probe));
    trigger_position.is_some_and(|trigger_position| {
        quiescence_position.is_none_or(|quiescence_position| trigger_position > quiescence_position)
    })
}

async fn start_idle_turn_after_finalization(codex: &CodexThread, input_text: &str) {
    let expected_input = vec![responses::user_message_item(input_text)];
    let mut input = expected_input.clone();
    timeout(CAPACITY_FIXTURE_TIMEOUT, async {
        loop {
            match codex.try_start_turn_if_idle(input).await {
                Ok(()) => break,
                Err(rejected) => {
                    assert_eq!(rejected.reason(), TryStartTurnIfIdleRejectionReason::Busy);
                    input = rejected.into_input();
                    assert_eq!(input, expected_input);
                    tokio::task::yield_now().await;
                }
            }
        }
    })
    .await
    .expect("automatic turn should start after finalization settles");
}

async fn spawn_capacity_worker(
    thread_manager: &ThreadManager,
    parent: &CodexThread,
    prompt: &str,
) -> Result<Arc<CodexThread>> {
    let mut created = thread_manager.subscribe_thread_created();
    start_idle_turn_after_finalization(parent, prompt).await;
    let worker_id = timeout(WORKER_CREATION_TIMEOUT, created.recv())
        .await
        .unwrap_or_else(|error| panic!("worker for {prompt:?} should be created: {error}"))
        .expect("thread creation channel should remain open");
    let worker = thread_manager.get_thread(worker_id).await?;
    tokio::join!(
        wait_for_event(parent, |event| matches!(event, EventMsg::TurnComplete(_))),
        wait_for_event(worker.as_ref(), |event| matches!(
            event,
            EventMsg::TurnComplete(_)
        )),
    );
    Ok(worker)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trigger_mail_waits_for_execution_capacity_and_starts_exactly_once() -> Result<()> {
    skip_if_no_network!(Ok(()));

    const FIRST_SPAWN_PROMPT: &str = "spawn the first capacity worker";
    const FIRST_SPAWN_TASK: &str = "first capacity worker task";
    const FIRST_SPAWN_CALL: &str = "spawn-capacity-first";
    const SECOND_SPAWN_PROMPT: &str = "spawn the trigger target worker";
    const SECOND_SPAWN_TASK: &str = "trigger target worker task";
    const SECOND_SPAWN_CALL: &str = "spawn-capacity-target";
    const PARENT_OWNER_INPUT: &str = "hold the parent execution lease";
    const CHILD_OWNER_INPUT: &str = "hold the child execution lease";
    const TRIGGER_CONTENT: &str = "run after capacity is released";
    const CAPACITY_WAIT_PROBE_INPUT: &str = "capacity wait path probe";
    const CAPACITY_QUIESCENCE_PROBE: &str = "verify the target start lane is quiescent";

    let server = start_mock_server().await;
    let first_spawn_args = serde_json::to_string(&json!({
        "message": FIRST_SPAWN_TASK,
        "task_name": "capacity_owner",
    }))?;
    let second_spawn_args = serde_json::to_string(&json!({
        "message": SECOND_SPAWN_TASK,
        "task_name": "capacity_target",
    }))?;

    let probe = Arc::new(PauseCapacityTurnStarts::default());
    let mut extensions = ExtensionRegistryBuilder::<Config>::new();
    extensions.turn_lifecycle_contributor(probe.clone());
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow collaboration");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow multi-agent V2");
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
        })
        .build_with_auto_env(&server)
        .await?;
    let capacity_parent = test
        .thread_manager
        .start_thread_with_options(StartThreadOptions {
            config: test.config.clone(),
            allow_provider_model_fallback: false,
            initial_history: InitialHistory::New,
            history_mode: None,
            session_source: Some(SessionSource::SubAgent(SubAgentSource::Other(
                "capacity fixture".to_string(),
            ))),
            thread_source: None,
            dynamic_tools: Vec::new(),
            metrics_service_name: None,
            parent_trace: None,
            environments: test.codex.environment_selections().await,
            thread_extension_init: Default::default(),
            supports_openai_form_elicitation: false,
        })
        .await?;

    let _first_spawn_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_SPAWN_PROMPT) && !body_contains(request, FIRST_SPAWN_CALL)
        },
        sse(vec![
            ev_response_created("first-spawn-response"),
            ev_function_call_with_namespace(
                FIRST_SPAWN_CALL,
                "collaboration",
                "spawn_agent",
                &first_spawn_args,
            ),
            ev_completed("first-spawn-response"),
        ]),
    )
    .await;
    let _first_worker_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FIRST_SPAWN_TASK) && !body_contains(request, FIRST_SPAWN_CALL)
        },
        sse_completed("first-worker-setup-completion"),
    )
    .await;
    let _first_parent_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FIRST_SPAWN_CALL),
        sse_completed("first-parent-setup-completion"),
    )
    .await;
    let first_worker = spawn_capacity_worker(
        test.thread_manager.as_ref(),
        capacity_parent.thread.as_ref(),
        FIRST_SPAWN_PROMPT,
    )
    .await?;

    let _second_spawn_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SECOND_SPAWN_PROMPT)
                && !body_contains(request, SECOND_SPAWN_CALL)
        },
        sse(vec![
            ev_response_created("second-spawn-response"),
            ev_function_call_with_namespace(
                SECOND_SPAWN_CALL,
                "collaboration",
                "spawn_agent",
                &second_spawn_args,
            ),
            ev_completed("second-spawn-response"),
        ]),
    )
    .await;
    let _second_worker_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SECOND_SPAWN_TASK) && !body_contains(request, SECOND_SPAWN_CALL)
        },
        sse_completed("second-worker-setup-completion"),
    )
    .await;
    let _second_parent_completion = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SECOND_SPAWN_CALL),
        sse_completed("second-parent-setup-completion"),
    )
    .await;
    let target_worker = spawn_capacity_worker(
        test.thread_manager.as_ref(),
        capacity_parent.thread.as_ref(),
        SECOND_SPAWN_PROMPT,
    )
    .await?;
    let setup_turn_starts = probe.calls.load(Ordering::SeqCst);

    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, PARENT_OWNER_INPUT),
        sse_completed("parent-owner-response"),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, CHILD_OWNER_INPUT),
        sse_completed("child-owner-response"),
    )
    .await;
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .and(|request: &wiremock::Request| {
            request_starts_trigger_turn(request, TRIGGER_CONTENT, CAPACITY_QUIESCENCE_PROBE)
        })
        .respond_with(responses::sse_response(sse_completed(
            "capacity-trigger-response",
        )))
        .mount(&server)
        .await;

    probe.armed.store(true, Ordering::SeqCst);
    let parent_for_start = Arc::clone(&capacity_parent.thread);
    let parent_start = tokio::spawn(async move {
        start_idle_turn_after_finalization(parent_for_start.as_ref(), PARENT_OWNER_INPUT).await;
    });
    let child_for_start = Arc::clone(&first_worker);
    let child_start = tokio::spawn(async move {
        start_idle_turn_after_finalization(child_for_start.as_ref(), CHILD_OWNER_INPUT).await;
    });
    timeout(TEST_TIMEOUT, async {
        loop {
            let notified = probe.paused_changed.notified();
            if probe.paused.load(Ordering::SeqCst) >= 2 {
                break;
            }
            notified.await;
        }
    })
    .await
    .expect("two limited turns should hold all execution leases");
    assert_eq!(probe.calls.load(Ordering::SeqCst), setup_turn_starts + 2);

    target_worker
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/capacity_sender")
                    .expect("capacity sender path should parse"),
                AgentPath::try_from("/root/capacity_target")
                    .expect("capacity target path should parse"),
                Vec::new(),
                TRIGGER_CONTENT.to_string(),
                /*trigger_turn*/ true,
            ),
        })
        .await?;
    target_worker
        .submit(Op::RealtimeConversationListVoices)
        .await?;
    wait_for_event(target_worker.as_ref(), |event| {
        matches!(event, EventMsg::RealtimeConversationListVoicesResponse(_))
    })
    .await;
    let capacity_wait_probe = responses::user_message_item(CAPACITY_WAIT_PROBE_INPUT);
    let rejected = timeout(
        TEST_TIMEOUT,
        target_worker.try_start_turn_if_idle(vec![capacity_wait_probe.clone()]),
    )
    .await
    .expect("capacity probe should reject instead of waiting inline")
    .expect_err("pending trigger mail should reject competing idle work");
    assert_eq!(
        rejected.reason(),
        TryStartTurnIfIdleRejectionReason::PendingTriggerTurn
    );
    assert_eq!(rejected.into_input(), vec![capacity_wait_probe]);
    assert_eq!(probe.calls.load(Ordering::SeqCst), setup_turn_starts + 2);
    assert!(
        server
            .received_requests()
            .await
            .expect("mock server should record requests")
            .iter()
            .all(|request| {
                !request_starts_trigger_turn(request, TRIGGER_CONTENT, CAPACITY_QUIESCENCE_PROBE)
            })
    );

    probe.release_one();
    wait_for_event(target_worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(probe.calls.load(Ordering::SeqCst), setup_turn_starts + 3);
    let requests = server
        .received_requests()
        .await
        .expect("mock server should record requests");
    let trigger_requests = requests
        .iter()
        .filter(|request| {
            request_starts_trigger_turn(request, TRIGGER_CONTENT, CAPACITY_QUIESCENCE_PROBE)
        })
        .collect::<Vec<_>>();
    assert_eq!(trigger_requests.len(), 1);
    let trigger_body = serde_json::from_slice::<Value>(&trigger_requests[0].body)?;
    let trigger_messages = trigger_body["input"]
        .as_array()
        .expect("trigger request should contain input")
        .iter()
        .filter(|message| {
            message.get("type").and_then(Value::as_str) == Some("agent_message")
                && message.get("author").and_then(Value::as_str) == Some("/root/capacity_sender")
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        strip_metadata_from_json(Value::Array(trigger_messages)),
        json!([{
            "type": "agent_message",
            "author": "/root/capacity_sender",
            "recipient": "/root/capacity_target",
            "content": [{"type": "input_text", "text": TRIGGER_CONTENT}],
        }])
    );
    assert!(!trigger_body.to_string().contains(CAPACITY_WAIT_PROBE_INPUT));

    probe.release_one();
    tokio::join!(
        wait_for_event(capacity_parent.thread.as_ref(), |event| matches!(
            event,
            EventMsg::TurnComplete(_)
        )),
        wait_for_event(first_worker.as_ref(), |event| matches!(
            event,
            EventMsg::TurnComplete(_)
        )),
    );
    timeout(TEST_TIMEOUT, parent_start)
        .await
        .expect("parent capacity turn should start after release")
        .expect("parent capacity task should not panic");
    timeout(TEST_TIMEOUT, child_start)
        .await
        .expect("child capacity turn should start after release")
        .expect("child capacity task should not panic");

    let quiescence_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, CAPACITY_QUIESCENCE_PROBE),
        sse_completed("capacity-quiescence-response"),
    )
    .await;
    start_idle_turn_after_finalization(target_worker.as_ref(), CAPACITY_QUIESCENCE_PROBE).await;
    wait_for_event(target_worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(probe.calls.load(Ordering::SeqCst), setup_turn_starts + 4);
    let quiescence_request = quiescence_response.single_request();
    assert_eq!(
        quiescence_request
            .message_input_texts("user")
            .last()
            .map(String::as_str),
        Some(CAPACITY_QUIESCENCE_PROBE)
    );

    target_worker.shutdown_and_wait().await?;
    assert_eq!(probe.calls.load(Ordering::SeqCst), setup_turn_starts + 4);
    let trigger_request_count = server
        .received_requests()
        .await
        .expect("mock server should record requests")
        .iter()
        .filter(|request| {
            request_starts_trigger_turn(request, TRIGGER_CONTENT, CAPACITY_QUIESCENCE_PROBE)
        })
        .count();
    assert_eq!(trigger_request_count, 1);
    Ok(())
}

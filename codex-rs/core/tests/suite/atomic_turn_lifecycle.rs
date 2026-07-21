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
use core_test_support::responses::mount_sse_sequence;
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

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct PauseFirstTurnStart {
    calls: AtomicUsize,
    first_entered: Notify,
    first_release: Notify,
}

impl codex_extension_api::TurnLifecycleContributor for PauseFirstTurnStart {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if self.calls.fetch_add(/*val*/ 1, Ordering::SeqCst) == 0 {
                self.first_entered.notify_one();
                self.first_release.notified().await;
            }
        })
    }
}

fn extensions_with_paused_first_start(
    probe: Arc<PauseFirstTurnStart>,
) -> Arc<codex_extension_api::ExtensionRegistry<Config>> {
    let mut builder = ExtensionRegistryBuilder::<Config>::new();
    builder.turn_lifecycle_contributor(probe);
    Arc::new(builder.build())
}

async fn wait_for_first_start(probe: &PauseFirstTurnStart) {
    timeout(TEST_TIMEOUT, probe.first_entered.notified())
        .await
        .expect("first turn should enter its lifecycle callback");
}

#[derive(Default)]
struct PauseCapacityTurnStarts {
    armed: AtomicBool,
    paused: AtomicUsize,
    paused_changed: Notify,
    release: Notify,
}

impl PauseCapacityTurnStarts {
    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

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

async fn spawn_capacity_worker(
    thread_manager: &ThreadManager,
    parent: &CodexThread,
    prompt: &str,
) -> Result<Arc<CodexThread>> {
    let mut created = thread_manager.subscribe_thread_created();
    parent
        .try_start_turn_if_idle(vec![responses::user_message_item(prompt)])
        .await
        .expect("worker spawn turn should start");
    let worker_id = timeout(TEST_TIMEOUT, created.recv())
        .await
        .expect("worker thread should be created")
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

async fn submit_trigger_mail_with_barrier(codex: &CodexThread, content: &str) -> Result<()> {
    codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/worker").expect("worker path should parse"),
                AgentPath::root(),
                Vec::new(),
                content.to_string(),
                /*trigger_turn*/ true,
            ),
        })
        .await?;
    codex.submit(Op::RealtimeConversationListVoices).await?;
    Ok(())
}

async fn wait_for_submission_barrier(codex: &CodexThread) {
    wait_for_event(codex, |event| {
        matches!(event, EventMsg::RealtimeConversationListVoicesResponse(_))
    })
    .await;
}

fn assert_single_trigger_request(
    response: &responses::ResponseMock,
    trigger_content: &str,
    excluded_idle_input: &str,
) {
    let request = response.single_request();
    assert_eq!(
        strip_metadata_from_json(Value::Array(request.inputs_of_type("agent_message"))),
        json!([{
            "type": "agent_message",
            "author": "/root/worker",
            "recipient": "/root",
            "content": [{"type": "input_text", "text": trigger_content}],
        }])
    );
    assert!(
        !request
            .message_input_texts("user")
            .iter()
            .any(|text| text == excluded_idle_input),
        "rejected idle input must not leak into the trigger turn"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successive_public_turns_release_the_lifecycle_slot() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("response-1"),
                ev_completed("response-1"),
            ]),
            sse(vec![
                ev_response_created("response-2"),
                ev_completed("response-2"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.codex
        .try_start_turn_if_idle(vec![responses::user_message_item("automatic turn")])
        .await
        .expect("idle automatic turn should start");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_turn("second turn").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    let first_user_messages = requests[0].message_input_texts("user");
    assert_eq!(
        first_user_messages.last().map(String::as_str),
        Some("automatic turn")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn triggering_agent_mail_starts_one_fifo_turn_and_releases_the_slot() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("trigger-response"),
                ev_completed("trigger-response"),
            ]),
            sse(vec![
                ev_response_created("public-response"),
                ev_completed("public-response"),
            ]),
        ],
    )
    .await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/first").expect("first author path should parse"),
                AgentPath::root(),
                Vec::new(),
                "queued first".to_string(),
                /*trigger_turn*/ false,
            ),
        })
        .await?;
    test.codex
        .submit(Op::InterAgentCommunication {
            communication: InterAgentCommunication::new(
                AgentPath::try_from("/root/second").expect("second author path should parse"),
                AgentPath::root(),
                Vec::new(),
                "trigger second".to_string(),
                /*trigger_turn*/ true,
            ),
        })
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let trigger_requests = responses.requests();
    assert_eq!(trigger_requests.len(), 1);
    assert_eq!(
        strip_metadata_from_json(Value::Array(
            trigger_requests[0].inputs_of_type("agent_message")
        )),
        json!([
            {
                "type": "agent_message",
                "author": "/root/first",
                "recipient": "/root",
                "content": [{"type": "input_text", "text": "queued first"}],
            },
            {
                "type": "agent_message",
                "author": "/root/second",
                "recipient": "/root",
                "content": [{"type": "input_text", "text": "trigger second"}],
            },
        ])
    );

    test.submit_turn("after triggered mail").await?;

    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .message_input_texts("user")
            .last()
            .map(String::as_str),
        Some("after triggered mail")
    );
    Ok(())
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

    let server = start_mock_server().await;
    let first_spawn_args = serde_json::to_string(&json!({
        "message": FIRST_SPAWN_TASK,
        "task_name": "capacity_owner",
    }))?;
    let second_spawn_args = serde_json::to_string(&json!({
        "message": SECOND_SPAWN_TASK,
        "task_name": "capacity_target",
    }))?;
    let _setup_responses = mount_sse_sequence(
        &server,
        vec![
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
            sse_completed("first-setup-completion"),
            sse_completed("second-setup-completion"),
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
            sse_completed("third-setup-completion"),
            sse_completed("fourth-setup-completion"),
        ],
    )
    .await;

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

    let first_worker = spawn_capacity_worker(
        test.thread_manager.as_ref(),
        capacity_parent.thread.as_ref(),
        FIRST_SPAWN_PROMPT,
    )
    .await?;
    let target_worker = spawn_capacity_worker(
        test.thread_manager.as_ref(),
        capacity_parent.thread.as_ref(),
        SECOND_SPAWN_PROMPT,
    )
    .await?;

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
    let trigger_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, TRIGGER_CONTENT),
        sse_completed("capacity-trigger-response"),
    )
    .await;

    probe.arm();
    let parent_for_start = Arc::clone(&capacity_parent.thread);
    let parent_start = tokio::spawn(async move {
        parent_for_start
            .try_start_turn_if_idle(vec![responses::user_message_item(PARENT_OWNER_INPUT)])
            .await
    });
    let child_for_start = Arc::clone(&first_worker);
    let child_start = tokio::spawn(async move {
        child_for_start
            .try_start_turn_if_idle(vec![responses::user_message_item(CHILD_OWNER_INPUT)])
            .await
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
    wait_for_submission_barrier(target_worker.as_ref()).await;
    assert!(trigger_response.requests().is_empty());

    probe.release_one();
    wait_for_event(target_worker.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = trigger_response.single_request();
    let trigger_messages = request
        .inputs_of_type("agent_message")
        .into_iter()
        .filter(|message| {
            message.get("author").and_then(Value::as_str) == Some("/root/capacity_sender")
        })
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
        .expect("parent capacity task should not panic")
        .expect("parent capacity turn should start");
    timeout(TEST_TIMEOUT, child_start)
        .await
        .expect("child capacity turn should start after release")
        .expect("child capacity task should not panic")
        .expect("child capacity turn should start");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trigger_mail_preempts_a_paused_public_idle_start() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("trigger-after-preemption"),
            ev_completed("trigger-after-preemption"),
        ]),
    )
    .await;
    let probe = Arc::new(PauseFirstTurnStart::default());
    let test = test_codex()
        .with_extensions(extensions_with_paused_first_start(Arc::clone(&probe)))
        .build_with_auto_env(&server)
        .await?;
    let idle_input = responses::user_message_item("idle input must be rejected");
    let codex_for_start = Arc::clone(&test.codex);
    let input_for_start = idle_input.clone();
    let starting = tokio::spawn(async move {
        codex_for_start
            .try_start_turn_if_idle(vec![input_for_start])
            .await
    });
    wait_for_first_start(probe.as_ref()).await;

    submit_trigger_mail_with_barrier(&test.codex, "preempting trigger").await?;
    wait_for_submission_barrier(&test.codex).await;
    probe.first_release.notify_one();

    let rejected = timeout(TEST_TIMEOUT, starting)
        .await
        .expect("preempted idle start should terminalize")
        .expect("preempted idle start task should not panic")
        .expect_err("pending trigger mail should preempt the idle start");
    assert_eq!(
        rejected.reason(),
        TryStartTurnIfIdleRejectionReason::PendingTriggerTurn
    );
    assert_eq!(rejected.into_input(), vec![idle_input]);
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_single_trigger_request(
        &response,
        "preempting trigger",
        "idle input must be rejected",
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_paused_public_idle_start_does_not_strand_trigger_mail() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("trigger-after-cancellation"),
            ev_completed("trigger-after-cancellation"),
        ]),
    )
    .await;
    let probe = Arc::new(PauseFirstTurnStart::default());
    let test = test_codex()
        .with_extensions(extensions_with_paused_first_start(Arc::clone(&probe)))
        .build_with_auto_env(&server)
        .await?;
    let codex_for_start = Arc::clone(&test.codex);
    let starting = tokio::spawn(async move {
        codex_for_start
            .try_start_turn_if_idle(vec![responses::user_message_item("cancelled idle input")])
            .await
    });
    wait_for_first_start(probe.as_ref()).await;

    submit_trigger_mail_with_barrier(&test.codex, "trigger survives cancellation").await?;
    wait_for_submission_barrier(&test.codex).await;
    starting.abort();
    let start_error = timeout(TEST_TIMEOUT, starting)
        .await
        .expect("cancelled idle start should terminalize")
        .expect_err("idle start task should be cancelled");
    assert!(start_error.is_cancelled());
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;

    assert_single_trigger_request(
        &response,
        "trigger survives cancellation",
        "cancelled idle input",
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_user_turn_replaces_a_paused_idle_start() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("public-user-replacement"),
            ev_completed("public-user-replacement"),
        ]),
    )
    .await;
    let probe = Arc::new(PauseFirstTurnStart::default());
    let test = test_codex()
        .with_extensions(extensions_with_paused_first_start(Arc::clone(&probe)))
        .build_with_auto_env(&server)
        .await?;
    let idle_input = responses::user_message_item("idle input must be replaced");
    let codex_for_start = Arc::clone(&test.codex);
    let input_for_start = idle_input.clone();
    let starting = tokio::spawn(async move {
        codex_for_start
            .try_start_turn_if_idle(vec![input_for_start])
            .await
    });
    wait_for_first_start(probe.as_ref()).await;

    let (public_result, idle_result) =
        tokio::join!(test.submit_turn("public user replacement"), async {
            timeout(TEST_TIMEOUT, starting)
                .await
                .expect("replaced idle start should terminalize")
                .expect("replaced idle start task should not panic")
                .expect_err("public user turn should replace the idle start")
        });
    public_result?;
    assert_eq!(
        idle_result.reason(),
        TryStartTurnIfIdleRejectionReason::Busy
    );
    assert_eq!(idle_result.into_input(), vec![idle_input]);

    let request = response.single_request();
    let user_messages = request.message_input_texts("user");
    assert_eq!(
        user_messages.last().map(String::as_str),
        Some("public user replacement")
    );
    assert!(
        !user_messages
            .iter()
            .any(|text| text == "idle input must be replaced"),
        "replaced idle input must not leak into the public user turn"
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupt_during_public_start_cancels_exact_generation_and_recovers() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let response = responses::mount_sse_once(
        &server,
        sse(vec![
            ev_response_created("response-after-interrupt"),
            ev_completed("response-after-interrupt"),
        ]),
    )
    .await;
    let probe = Arc::new(PauseFirstTurnStart::default());
    let test = test_codex()
        .with_extensions(extensions_with_paused_first_start(Arc::clone(&probe)))
        .build_with_auto_env(&server)
        .await?;
    let codex_for_start = Arc::clone(&test.codex);
    let starting = tokio::spawn(async move {
        codex_for_start
            .try_start_turn_if_idle(vec![responses::user_message_item("cancelled start")])
            .await
    });
    wait_for_first_start(probe.as_ref()).await;

    test.codex.submit(Op::Interrupt).await?;
    let rejected = timeout(TEST_TIMEOUT, starting)
        .await
        .expect("interrupted start should terminalize")
        .expect("interrupted start task should not panic")
        .expect_err("interrupted automatic start should reject its input");
    assert_eq!(
        rejected.into_input(),
        vec![responses::user_message_item("cancelled start")]
    );

    test.codex
        .try_start_turn_if_idle(vec![responses::user_message_item("after interrupt")])
        .await
        .expect("a later automatic turn should start");
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(response.requests().len(), 1);
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_during_public_start_prevents_commit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let probe = Arc::new(PauseFirstTurnStart::default());
    let test = test_codex()
        .with_extensions(extensions_with_paused_first_start(Arc::clone(&probe)))
        .build_with_auto_env(&server)
        .await?;
    let codex_for_start = Arc::clone(&test.codex);
    let starting = tokio::spawn(async move {
        codex_for_start
            .try_start_turn_if_idle(vec![responses::user_message_item("shutdown start")])
            .await
    });
    wait_for_first_start(probe.as_ref()).await;

    test.codex.shutdown_and_wait().await?;
    let rejected = timeout(TEST_TIMEOUT, starting)
        .await
        .expect("shutdown start should terminalize")
        .expect("shutdown start task should not panic")
        .expect_err("shutdown should reject an uncommitted automatic start");
    assert_eq!(
        rejected.into_input(),
        vec![responses::user_message_item("shutdown start")]
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

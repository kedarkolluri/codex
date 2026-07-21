use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TryStartTurnIfIdleRejectionReason;
use codex_core::config::Config;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_protocol::AgentPath;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use core_test_support::responses;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_metadata_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
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
async fn dropping_a_paused_idle_start_caller_does_not_strand_trigger_mail() -> Result<()> {
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
async fn interrupt_during_a_paused_idle_start_rejects_input_and_releases_the_slot() -> Result<()> {
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
    assert_eq!(rejected.reason(), TryStartTurnIfIdleRejectionReason::Busy);
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

    let request = response.single_request();
    let user_messages = request.message_input_texts("user");
    assert_eq!(
        user_messages.last().map(String::as_str),
        Some("after interrupt")
    );
    assert!(
        !user_messages.iter().any(|text| text == "cancelled start"),
        "interrupted input must not leak into the recovered turn"
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    Ok(())
}

use std::time::Duration;

use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_features::Feature;
use codex_login::CodexAuth;
use tokio::time::timeout;

use crate::agent::control::AgentExecutionAdmission;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;

#[tokio::test]
async fn trigger_turn_capacity_retries_coalesce_and_start_after_release() {
    let (session, _, rx_event) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| {
            let _ = config.features.enable(Feature::MultiAgentV2);
        },
    )
    .await;
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    session
        .state
        .lock()
        .await
        .session_configuration
        .session_source = source.clone();
    let control = session
        .services
        .agent_control
        .clone()
        .with_session_id(SessionId::default(), /*max_threads*/ 1);

    let guard = match control.execution_admission(MultiAgentVersion::V2, &source) {
        AgentExecutionAdmission::Admitted(guard) => guard,
        AgentExecutionAdmission::Unrestricted | AgentExecutionAdmission::AtCapacity(_) => {
            panic!("first limited turn should be admitted")
        }
    };
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            AgentPath::root(),
            AgentPath::root(),
            Vec::new(),
            "capacity retry".to_string(),
            /*trigger_turn*/ true,
        ))
        .await;

    session
        .maybe_start_turn_for_pending_work_with_sub_id("first-attempt".to_string())
        .await;
    // A second automatic wakeup must coalesce with the already waiting retry.
    session
        .maybe_start_turn_for_pending_work_with_sub_id("coalesced-attempt".to_string())
        .await;
    assert!(session.trigger_turn_retry.is_waiting());
    assert!(session.active_turn.lock().await.is_idle());
    assert!(session.input_queue.has_trigger_turn_mailbox_items().await);
    while let Ok(event) = rx_event.try_recv() {
        assert!(
            !matches!(event.msg, EventMsg::TurnStarted(_)),
            "capacity-rejected mailbox work must not start a task"
        );
    }

    drop(guard);
    timeout(Duration::from_secs(/*secs*/ 5), async {
        loop {
            let event = rx_event.recv().await.expect("session event stream open");
            if matches!(event.msg, EventMsg::TurnStarted(_)) {
                break;
            }
        }
    })
    .await
    .expect("capacity release should start one coalesced pending mailbox turn");
    assert!(!session.input_queue.has_trigger_turn_mailbox_items().await);

    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}

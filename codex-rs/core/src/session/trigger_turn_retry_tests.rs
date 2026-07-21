use std::sync::Arc;
use std::sync::Barrier;
use std::sync::OnceLock;
use std::time::Duration;

use codex_protocol::AgentPath;
use codex_protocol::SessionId;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use pretty_assertions::assert_eq;
use tokio::time::timeout;

use super::AutomaticTicketInvalidation;
use super::PendingWorkStartRequest;
use super::TriggerTurnRetry;
use crate::agent::AgentControl;
use crate::session::tests::make_session_and_context;
use crate::session::turn_start_gate::TurnStartGate;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn capacity_wait_keeps_latest_request_and_retry_dominates_suppression() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = TriggerTurnRetry::default();
    let claim = retry
        .register_capacity_wait(PendingWorkStartRequest::new(
            "first".to_string(),
            ticket,
            AutomaticTicketInvalidation::Suppress,
        ))
        .expect("first capacity request should own the wait");

    assert!(
        retry
            .register_capacity_wait(PendingWorkStartRequest::new(
                "newest".to_string(),
                ticket,
                AutomaticTicketInvalidation::Retry,
            ))
            .is_none()
    );
    let (sub_id, retained_ticket, invalidation) = claim.take_request().into_parts();
    assert_eq!(
        (sub_id, retained_ticket, invalidation),
        (
            "newest".to_string(),
            ticket,
            AutomaticTicketInvalidation::Retry,
        )
    );
}

#[test]
fn newer_authenticated_epoch_replaces_waiting_request() {
    let gate = TurnStartGate::default();
    let old_ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = TriggerTurnRetry::default();
    let claim = retry
        .register_capacity_wait(PendingWorkStartRequest::new(
            "old".to_string(),
            old_ticket,
            AutomaticTicketInvalidation::Retry,
        ))
        .expect("first capacity request should own the wait");
    let current_ticket = gate.suppress_automatic_starts();

    assert!(
        retry
            .register_capacity_wait(PendingWorkStartRequest::new(
                "current".to_string(),
                current_ticket,
                AutomaticTicketInvalidation::Suppress,
            ))
            .is_none()
    );
    let (sub_id, retained_ticket, invalidation) = claim.take_request().into_parts();
    assert_eq!(
        (sub_id, retained_ticket, invalidation),
        (
            "current".to_string(),
            current_ticket,
            AutomaticTicketInvalidation::Suppress,
        )
    );
}

#[test]
fn dropping_capacity_claim_rearms_wait_registration() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = TriggerTurnRetry::default();
    let claim = retry
        .register_capacity_wait(PendingWorkStartRequest::new(
            "first".to_string(),
            ticket,
            AutomaticTicketInvalidation::Retry,
        ))
        .expect("first capacity request should own the wait");

    drop(claim);

    assert!(
        retry
            .register_capacity_wait(PendingWorkStartRequest::new(
                "second".to_string(),
                ticket,
                AutomaticTicketInvalidation::Retry,
            ))
            .is_some()
    );
}

#[test]
fn concurrent_capacity_requests_choose_exactly_one_wait_owner() {
    let gate = TurnStartGate::default();
    let ticket = gate.automatic_start_ticket().expect("gate open");
    let retry = Arc::new(TriggerTurnRetry::default());
    let start = Arc::new(Barrier::new(/*n*/ 3));
    let finish = Arc::new(Barrier::new(/*n*/ 3));
    let handles: [_; 2] = std::array::from_fn(|index| {
        let retry = Arc::clone(&retry);
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        std::thread::spawn(move || {
            start.wait();
            let claim = retry.register_capacity_wait(PendingWorkStartRequest::new(
                index.to_string(),
                ticket,
                AutomaticTicketInvalidation::Retry,
            ));
            finish.wait();
            claim
        })
    });

    start.wait();
    finish.wait();
    let mut claims = handles.map(|handle| {
        handle
            .join()
            .expect("capacity retry contender should not panic")
    });
    claims.sort_by_key(Option::is_some);
    assert!(claims[0].is_none());
    assert!(claims[1].is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn released_capacity_retries_and_commits_trigger_mail() {
    let (mut session, _turn_context) = make_session_and_context().await;
    let source = SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()));
    session.multi_agent_version = OnceLock::from(MultiAgentVersion::V2);
    session.state.get_mut().session_configuration.session_source = source.clone();
    session.services.agent_control =
        AgentControl::default().with_session_id(SessionId::new(), /*max_threads*/ 1);
    let held_capacity = session
        .services
        .agent_control
        .execution_guard(MultiAgentVersion::V2, &source)
        .expect("limited subagent turn should hold capacity");
    let session = Arc::new(session);
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("worker path should parse"),
            AgentPath::root(),
            Vec::new(),
            "capacity update".to_string(),
            /*trigger_turn*/ true,
        ))
        .await;

    session
        .maybe_start_turn_for_pending_work_with_sub_id("capacity-retry".to_string())
        .await;
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
    assert!(session.input_queue.has_trigger_turn_mailbox_items().await);

    drop(held_capacity);
    timeout(TEST_TIMEOUT, async {
        while session.input_queue.has_trigger_turn_mailbox_items().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("capacity release should retry and attach trigger mail");
    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Replaced)
        .await;
}

use std::sync::Arc;

use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;

use crate::session::tests::make_session_and_context;

#[tokio::test]
async fn idle_interrupt_preserves_trigger_mail_and_suppresses_automatic_start() {
    let (session, _turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("worker path should parse"),
            AgentPath::root(),
            Vec::new(),
            "queued update".to_string(),
            /*trigger_turn*/ true,
        ))
        .await;
    let interrupted_ticket = session
        .turn_start_gate
        .automatic_start_ticket()
        .expect("automatic-start gate should be open");

    session.interrupt_task().await;

    assert!(session.input_queue.has_trigger_turn_mailbox_items().await);
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
    assert!(
        !session
            .turn_start_gate
            .admits_automatic_start(interrupted_ticket)
    );
    assert_eq!(
        session
            .turn_start_gate
            .retry_ticket_after_invalidation(interrupted_ticket),
        None
    );
}

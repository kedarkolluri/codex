use std::time::Duration;

use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::InputQueue;
use super::InputQueueActivity;
use super::TurnInput;
use crate::state::MailboxDeliveryPhase;
use crate::state::TurnState;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

fn communication(message: &str, trigger_turn: bool) -> InterAgentCommunication {
    InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        message.to_string(),
        trigger_turn,
    )
}

fn user_input(text: &str) -> TurnInput {
    TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: text.to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }
}

#[tokio::test]
async fn dropping_prepared_start_input_preserves_existing_input_and_restores_mailbox_fifo() {
    let queue = InputQueue::new();
    let (mut activity_rx, pending_activity) = queue.subscribe_activity(/*turn_state*/ None).await;
    assert_eq!(pending_activity, None);
    let first = communication("first", /*trigger_turn*/ false);
    let second = communication("second", /*trigger_turn*/ true);
    queue.enqueue_mailbox_communication(first.clone()).await;
    queue.enqueue_mailbox_communication(second.clone()).await;
    activity_rx
        .changed()
        .await
        .expect("original mailbox activity should be observed");
    assert_eq!(
        *activity_rx.borrow_and_update(),
        InputQueueActivity::Mailbox
    );
    let turn_state = Mutex::new(TurnState::default());
    let existing = user_input("existing");
    turn_state
        .lock()
        .await
        .pending_input
        .items
        .push(existing.clone());

    let prepared = queue
        .prepare_starting_turn_input(&turn_state, vec![user_input("explicit")])
        .await;
    assert!(prepared.has_trigger_turn_mailbox_items());
    drop(prepared);

    timeout(TEST_TIMEOUT, activity_rx.changed())
        .await
        .expect("rollback should promptly publish fresh mailbox activity")
        .expect("mailbox activity sender should remain open");
    assert_eq!(
        *activity_rx.borrow_and_update(),
        InputQueueActivity::Mailbox
    );

    assert_eq!(turn_state.lock().await.pending_input.items, vec![existing]);
    assert_eq!(
        queue.drain_mailbox_input_items().await,
        vec![
            TurnInput::InterAgentCommunication(first),
            TurnInput::InterAgentCommunication(second),
        ]
    );
}

#[tokio::test]
async fn trigger_detection_only_considers_the_attached_mailbox_suffix() {
    let queue = InputQueue::new();
    let mail = communication("non-trigger mailbox", /*trigger_turn*/ false);
    queue.enqueue_mailbox_communication(mail.clone()).await;
    let turn_state = Mutex::new(TurnState::default());
    let existing_trigger = TurnInput::InterAgentCommunication(communication(
        "existing trigger",
        /*trigger_turn*/ true,
    ));
    turn_state
        .lock()
        .await
        .pending_input
        .items
        .push(existing_trigger.clone());
    let explicit_trigger = TurnInput::InterAgentCommunication(communication(
        "explicit trigger",
        /*trigger_turn*/ true,
    ));

    let prepared = queue
        .prepare_starting_turn_input(&turn_state, vec![explicit_trigger.clone()])
        .await;
    assert!(!prepared.has_trigger_turn_mailbox_items());
    prepared.commit();

    assert_eq!(
        turn_state.lock().await.pending_input.items,
        vec![
            existing_trigger,
            explicit_trigger,
            TurnInput::InterAgentCommunication(mail),
        ]
    );
}

#[tokio::test]
async fn committing_prepared_start_input_moves_explicit_then_mailbox_input() {
    let queue = InputQueue::new();
    let mail = communication("mail", /*trigger_turn*/ true);
    queue.enqueue_mailbox_communication(mail.clone()).await;
    let turn_state = Mutex::new(TurnState::default());
    let explicit = user_input("explicit");

    queue
        .prepare_starting_turn_input(&turn_state, vec![explicit.clone()])
        .await
        .commit();

    assert_eq!(
        turn_state.lock().await.pending_input.items,
        vec![explicit, TurnInput::InterAgentCommunication(mail)]
    );
    assert_eq!(
        queue.drain_mailbox_input_items().await,
        Vec::<TurnInput>::new()
    );
}

#[tokio::test]
async fn next_turn_preparation_commits_explicit_input_without_attaching_mailbox() {
    let queue = InputQueue::new();
    let mail = communication("mail", /*trigger_turn*/ true);
    queue.enqueue_mailbox_communication(mail.clone()).await;
    let turn_state = Mutex::new(TurnState::default());
    turn_state
        .lock()
        .await
        .set_mailbox_delivery_phase(MailboxDeliveryPhase::NextTurn);
    let explicit = user_input("explicit");

    let prepared = queue
        .prepare_starting_turn_input(&turn_state, vec![explicit.clone()])
        .await;
    assert!(!prepared.has_trigger_turn_mailbox_items());
    prepared.commit();

    assert_eq!(turn_state.lock().await.pending_input.items, vec![explicit]);
    assert_eq!(
        queue.drain_mailbox_input_items().await,
        vec![TurnInput::InterAgentCommunication(mail)]
    );
}

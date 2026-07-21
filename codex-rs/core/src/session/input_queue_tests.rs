use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;

use super::InputQueue;
use super::TurnInput;
use crate::state::TurnState;

fn communication(message: &str) -> InterAgentCommunication {
    InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        message.to_string(),
        /*trigger_turn*/ true,
    )
}

fn explicit_input() -> TurnInput {
    TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "explicit".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
    }
}

#[tokio::test]
async fn dropping_prepared_start_input_restores_mailbox_fifo() {
    let queue = InputQueue::new();
    let first = communication("first");
    let second = communication("second");
    queue.enqueue_mailbox_communication(first.clone()).await;
    queue.enqueue_mailbox_communication(second.clone()).await;
    let turn_state = Mutex::new(TurnState::default());

    let prepared = queue
        .prepare_starting_turn_input(&turn_state, vec![explicit_input()])
        .await;
    drop(prepared);

    assert_eq!(
        queue.drain_mailbox_input_items().await,
        vec![
            TurnInput::InterAgentCommunication(first),
            TurnInput::InterAgentCommunication(second),
        ]
    );
    assert_eq!(
        turn_state.lock().await.pending_input.items,
        Vec::<TurnInput>::new()
    );
}

#[tokio::test]
async fn committing_prepared_start_input_moves_explicit_then_mailbox_input() {
    let queue = InputQueue::new();
    let mail = communication("mail");
    queue.enqueue_mailbox_communication(mail.clone()).await;
    let turn_state = Mutex::new(TurnState::default());
    let explicit = explicit_input();

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

use super::*;
use crate::session::input_queue::PendingInputClaim;
use crate::session::input_queue::PendingInputClaimMode;
use crate::session::input_queue::PendingInputRecordingResult;
use pretty_assertions::assert_eq;

fn response_item(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

#[tokio::test]
async fn pending_input_claim_authenticates_and_preserves_fifo() {
    let (session, turn_context, _events_rx) = make_session_and_context_with_rx().await;
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: true,
            },
        )
        .await;
    let local = response_item("turn-local input");
    let mail = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "mailbox input".to_string(),
        /*trigger_turn*/ false,
    );
    assert_eq!(session.inject_if_running(vec![local.clone()]).await, Ok(()));
    session
        .input_queue
        .enqueue_mailbox_communication(mail.clone())
        .await;

    let other_turn = session
        .new_default_turn_with_sub_id("other-turn".to_string())
        .await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(
                &session.active_turn,
                &other_turn,
                PendingInputClaimMode::CurrentTurn,
            )
            .await,
        PendingInputClaim::Inactive
    ));

    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_turn(
            &session.active_turn,
            &turn_context,
            PendingInputClaimMode::CurrentTurn,
        )
        .await
    else {
        panic!("the exact running turn should acquire pending input");
    };
    let recording = claim.recording();
    let PendingInputClaim::Recording(joined_recording) = session
        .input_queue
        .claim_pending_input_for_turn(
            &session.active_turn,
            &turn_context,
            PendingInputClaimMode::CurrentTurn,
        )
        .await
    else {
        panic!("a concurrent exact claim should join the active recording");
    };
    let (items, completion) = claim.into_parts();
    assert_eq!(
        items,
        vec![
            TurnInput::ResponseItem(local),
            TurnInput::InterAgentCommunication(mail),
        ]
    );
    let completed = PendingInputRecordingResult::Completed {
        should_stop: false,
    };
    completion.finish(completed).await;
    assert_eq!(recording.wait().await, completed);
    assert_eq!(joined_recording.wait().await, completed);

    session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
}

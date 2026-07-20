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
async fn pending_input_claim_authenticates_competing_claims_and_preserves_fifo() {
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
    let local_one = response_item("turn-local input one");
    let local_two = response_item("turn-local input two");
    let mail_one = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "mailbox input one".to_string(),
        /*trigger_turn*/ false,
    );
    let mail_two = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "mailbox input two".to_string(),
        /*trigger_turn*/ false,
    );
    assert_eq!(
        session
            .inject_if_running(vec![local_one.clone(), local_two.clone()])
            .await,
        Ok(())
    );
    session
        .input_queue
        .enqueue_mailbox_communication(mail_one.clone())
        .await;
    session
        .input_queue
        .enqueue_mailbox_communication(mail_two.clone())
        .await;

    let other_turn = session
        .new_default_turn_with_sub_id(turn_context.sub_id.clone())
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

    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let first_session = Arc::clone(&session);
    let first_turn_context = Arc::clone(&turn_context);
    let first_barrier = Arc::clone(&barrier);
    let first = tokio::spawn(async move {
        first_barrier.wait().await;
        first_session
            .input_queue
            .claim_pending_input_for_turn(
                &first_session.active_turn,
                &first_turn_context,
                PendingInputClaimMode::CurrentTurn,
            )
            .await
    });
    let second_session = Arc::clone(&session);
    let second_turn_context = Arc::clone(&turn_context);
    let second_barrier = Arc::clone(&barrier);
    let second = tokio::spawn(async move {
        second_barrier.wait().await;
        second_session
            .input_queue
            .claim_pending_input_for_turn(
                &second_session.active_turn,
                &second_turn_context,
                PendingInputClaimMode::CurrentTurn,
            )
            .await
    });
    barrier.wait().await;
    let first = first.await.expect("first claim task should complete");
    let second = second.await.expect("second claim task should complete");
    let (claim, joined_recording) = match (first, second) {
        (PendingInputClaim::Acquired(claim), PendingInputClaim::Recording(recording))
        | (PendingInputClaim::Recording(recording), PendingInputClaim::Acquired(claim)) => {
            (claim, recording)
        }
        _ => panic!("exact competing claims should acquire once and join once"),
    };
    let recording = claim.recording();
    let (items, completion) = claim.into_parts();
    assert_eq!(
        items.as_ref(),
        &[
            TurnInput::ResponseItem(local_one),
            TurnInput::ResponseItem(local_two),
            TurnInput::InterAgentCommunication(mail_one),
            TurnInput::InterAgentCommunication(mail_two),
        ]
    );
    let should_stop = false;
    completion.complete(should_stop).await;
    let completed = PendingInputRecordingResult::Completed { should_stop };
    assert_eq!(recording.wait().await, completed);
    assert_eq!(joined_recording.wait().await, completed);
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(
                &session.active_turn,
                &turn_context,
                PendingInputClaimMode::CurrentTurn,
            )
            .await,
        PendingInputClaim::Empty
    ));

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn pending_input_claim_defers_mailbox_until_delivery_is_accepted() {
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
    let turn_state = {
        let active = session.active_turn.lock().await;
        Arc::clone(
            &active
                .as_ref()
                .expect("test task should own the active turn")
                .turn_state,
        )
    };
    session
        .input_queue
        .defer_mailbox_delivery_to_next_turn(&session.active_turn, &turn_context.sub_id)
        .await;
    let local = response_item("finalization-only input");
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(local.clone())],
        )
        .await;
    let mail = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "next-turn mailbox input".to_string(),
        /*trigger_turn*/ false,
    );
    session
        .input_queue
        .enqueue_mailbox_communication(mail.clone())
        .await;

    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(
                &session.active_turn,
                &turn_context,
                PendingInputClaimMode::CurrentTurn,
            )
            .await,
        PendingInputClaim::Empty
    ));
    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_turn(
            &session.active_turn,
            &turn_context,
            PendingInputClaimMode::Finalization,
        )
        .await
    else {
        panic!("finalization should claim only turn-local input");
    };
    let recording = claim.recording();
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::ResponseItem(local)]);
    let should_stop = false;
    completion.complete(should_stop).await;
    let completed = PendingInputRecordingResult::Completed { should_stop };
    assert_eq!(recording.wait().await, completed);
    assert!(
        session
            .input_queue
            .pending_input_recording(turn_state.as_ref())
            .await
            .is_none()
    );

    session
        .input_queue
        .accept_mailbox_delivery_for_current_turn(&session.active_turn, &turn_context.sub_id)
        .await;
    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_turn(
            &session.active_turn,
            &turn_context,
            PendingInputClaimMode::CurrentTurn,
        )
        .await
    else {
        panic!("accepted next-turn mailbox input should become claimable");
    };
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::InterAgentCommunication(mail)]);
    completion.complete(should_stop).await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(
                &session.active_turn,
                &turn_context,
                PendingInputClaimMode::CurrentTurn,
            )
            .await,
        PendingInputClaim::Empty
    ));

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn pending_input_claim_failure_retains_exact_displaced_turn_ownership() {
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
    let turn_state = {
        let active = session.active_turn.lock().await;
        Arc::clone(
            &active
                .as_ref()
                .expect("test task should own the active turn")
                .turn_state,
        )
    };

    let failed = response_item("retained after recorder failure");
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(failed.clone())],
        )
        .await;
    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_displaced_turn(&turn_state, &turn_context)
        .await
    else {
        panic!("the exact displaced turn should claim its local failure batch");
    };
    let failed_recording = claim.recording();
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::ResponseItem(failed.clone())]);
    let other_turn = session
        .new_default_turn_with_sub_id(turn_context.sub_id.clone())
        .await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_displaced_turn(&turn_state, &other_turn)
            .await,
        PendingInputClaim::Inactive
    ));
    drop(completion);
    assert_eq!(
        failed_recording.wait().await,
        PendingInputRecordingResult::Failed
    );
    let PendingInputClaim::Recording(retained) = session
        .input_queue
        .claim_pending_input_for_turn(
            &session.active_turn,
            &turn_context,
            PendingInputClaimMode::Finalization,
        )
        .await
    else {
        panic!("an exact claimant should rejoin the sticky failed recording");
    };
    assert_eq!(
        retained.claimed_input().as_ref(),
        &[TurnInput::ResponseItem(failed)]
    );
    assert_eq!(retained.wait().await, PendingInputRecordingResult::Failed);
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_displaced_turn(&turn_state, &turn_context)
            .await,
        PendingInputClaim::Recording(_)
    ));
    assert!(!turn_state.lock().await.pending_input.is_empty_and_idle());

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

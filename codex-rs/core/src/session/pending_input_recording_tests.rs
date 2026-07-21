use super::*;
use crate::session::input_queue::PendingInputClaim;
use crate::session::input_queue::PendingInputRecordingResult;
use crate::state::TurnState;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;

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

async fn active_turn_state(session: &Session) -> Arc<Mutex<TurnState>> {
    let active = session.active_turn.lock().await;
    Arc::clone(
        &active
            .as_ref()
            .expect("test task should own the active turn")
            .turn_state,
    )
}

#[tokio::test]
async fn pending_input_claim_authenticates_competitors_and_preserves_fifo_and_residuals() {
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
    session
        .input_queue
        .enqueue_mailbox_communication(mail_one.clone())
        .await;
    session
        .input_queue
        .enqueue_mailbox_communication(mail_two.clone())
        .await;
    session
        .input_queue
        .defer_mailbox_delivery_to_next_turn(&session.active_turn, &turn_context.sub_id)
        .await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(&session.active_turn, &turn_context)
            .await,
        PendingInputClaim::Empty
    ));
    assert!(session.input_queue.has_pending_mailbox_items().await);
    session
        .input_queue
        .accept_mailbox_delivery_for_current_turn(&session.active_turn, &turn_context.sub_id)
        .await;
    assert_eq!(
        session
            .inject_if_running(vec![local_one.clone(), local_two.clone()])
            .await,
        Ok(())
    );

    let other_turn = session
        .new_default_turn_with_sub_id(turn_context.sub_id.clone())
        .await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(&session.active_turn, &other_turn)
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
            .claim_pending_input_for_turn(&first_session.active_turn, &first_turn_context)
            .await
    });
    let second_session = Arc::clone(&session);
    let second_turn_context = Arc::clone(&turn_context);
    let second_barrier = Arc::clone(&barrier);
    let second = tokio::spawn(async move {
        second_barrier.wait().await;
        second_session
            .input_queue
            .claim_pending_input_for_turn(&second_session.active_turn, &second_turn_context)
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

    let late = response_item("turn-local input after claim");
    assert_eq!(session.inject_if_running(vec![late.clone()]).await, Ok(()));
    let PendingInputClaim::Recording(residual_waiter) = session
        .input_queue
        .claim_pending_input_for_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("a residual claimant should join the active recording");
    };
    assert_eq!(residual_waiter.claimed_input(), Arc::clone(&items));

    let should_stop = false;
    completion.complete(should_stop).await;
    let completed = PendingInputRecordingResult::Completed { should_stop };
    assert_eq!(recording.wait().await, completed);
    assert_eq!(joined_recording.wait().await, completed);
    assert_eq!(residual_waiter.wait().await, completed);

    let PendingInputClaim::Acquired(residual_claim) = session
        .input_queue
        .claim_pending_input_for_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("input arriving during recording should become the next claim");
    };
    let residual_recording = residual_claim.recording();
    let (residual_items, residual_completion) = residual_claim.into_parts();
    assert_eq!(residual_items.as_ref(), &[TurnInput::ResponseItem(late)]);
    residual_completion.complete(should_stop).await;
    assert_eq!(residual_recording.wait().await, completed);
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(&session.active_turn, &turn_context)
            .await,
        PendingInputClaim::Empty
    ));

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn pending_input_claim_failure_retains_exact_turn_ownership() {
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
    let failed = response_item("retained after recorder failure");
    assert_eq!(
        session.inject_if_running(vec![failed.clone()]).await,
        Ok(())
    );
    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("the exact running turn should claim its failure batch");
    };
    let failed_recording = claim.recording();
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::ResponseItem(failed.clone())]);

    let other_turn = session
        .new_default_turn_with_sub_id(turn_context.sub_id.clone())
        .await;
    {
        let mut active = session.active_turn.lock().await;
        let running = active
            .as_mut()
            .and_then(|active| active.task.as_mut())
            .expect("the original task should still be running");
        running.turn_context = Arc::clone(&other_turn);
    }
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(&session.active_turn, &other_turn)
            .await,
        PendingInputClaim::Inactive
    ));
    {
        let mut active = session.active_turn.lock().await;
        let running = active
            .as_mut()
            .and_then(|active| active.task.as_mut())
            .expect("the original task should still be running");
        running.turn_context = Arc::clone(&turn_context);
    }

    drop(completion);
    assert_eq!(
        failed_recording.wait().await,
        PendingInputRecordingResult::Failed
    );
    let PendingInputClaim::Recording(retained) = session
        .input_queue
        .claim_pending_input_for_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("an exact claimant should rejoin the sticky failed recording");
    };
    assert_eq!(
        retained.claimed_input().as_ref(),
        &[TurnInput::ResponseItem(failed)]
    );
    assert_eq!(retained.wait().await, PendingInputRecordingResult::Failed);

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn finalizing_claim_drains_local_residuals_without_consuming_next_turn_mail() {
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
    let turn_state = active_turn_state(&session).await;
    let local_one = response_item("finalizing local input one");
    let local_two = response_item("finalizing local input two");
    let mail = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "next-turn mailbox input".to_string(),
        /*trigger_turn*/ false,
    );
    session
        .input_queue
        .defer_mailbox_delivery_to_next_turn(&session.active_turn, &turn_context.sub_id)
        .await;
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(local_one.clone())],
        )
        .await;
    session
        .input_queue
        .enqueue_mailbox_communication(mail.clone())
        .await;

    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_turn(&session.active_turn, &turn_context)
            .await,
        PendingInputClaim::Empty
    ));
    let other_turn = session
        .new_default_turn_with_sub_id(turn_context.sub_id.clone())
        .await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_finalizing_turn(&session.active_turn, &other_turn)
            .await,
        PendingInputClaim::Inactive
    ));
    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_finalizing_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("the exact finalizing turn should claim its local input");
    };
    let recording = claim.recording();
    let (items, completion) = claim.into_parts();
    assert_eq!(
        items.as_ref(),
        &[TurnInput::ResponseItem(local_one.clone())]
    );
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(local_two.clone())],
        )
        .await;
    let PendingInputClaim::Recording(joined) = session
        .input_queue
        .claim_pending_input_for_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("a current-turn claimant should join the installed finalizing recording");
    };
    assert_eq!(joined.claimed_input(), Arc::clone(&items));
    let should_stop = false;
    completion.complete(should_stop).await;
    let completed = PendingInputRecordingResult::Completed { should_stop };
    assert_eq!(recording.wait().await, completed);
    assert_eq!(joined.wait().await, completed);

    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_finalizing_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("input appended during recording should become the next finalizing claim");
    };
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::ResponseItem(local_two)]);
    completion.complete(should_stop).await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_finalizing_turn(&session.active_turn, &turn_context)
            .await,
        PendingInputClaim::Empty
    ));
    assert!(session.input_queue.has_pending_mailbox_items().await);

    session
        .input_queue
        .accept_mailbox_delivery_for_current_turn(&session.active_turn, &turn_context.sub_id)
        .await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_finalizing_turn(&session.active_turn, &turn_context)
            .await,
        PendingInputClaim::Empty
    ));
    assert!(session.input_queue.has_pending_mailbox_items().await);
    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_turn(&session.active_turn, &turn_context)
        .await
    else {
        panic!("reopened current-turn delivery should claim the retained mailbox input");
    };
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::InterAgentCommunication(mail)]);
    completion.complete(should_stop).await;

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

#[tokio::test]
async fn displaced_claim_is_exact_local_only_and_sticky() {
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
    let turn_state = active_turn_state(&session).await;
    let local_one = response_item("displaced local input one");
    let local_two = response_item("displaced local input two");
    let failed = response_item("displaced sticky failure input");
    let mail = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "successor-owned mailbox input".to_string(),
        /*trigger_turn*/ false,
    );
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(local_one.clone())],
        )
        .await;
    session
        .input_queue
        .enqueue_mailbox_communication(mail)
        .await;

    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_displaced_turn(&turn_state, &turn_context)
        .await
    else {
        panic!("the displaced turn should claim its local input");
    };
    let recording = claim.recording();
    let (items, completion) = claim.into_parts();
    assert_eq!(
        items.as_ref(),
        &[TurnInput::ResponseItem(local_one.clone())]
    );
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            turn_state.as_ref(),
            vec![TurnInput::ResponseItem(local_two.clone())],
        )
        .await;
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
    let PendingInputClaim::Recording(joined) = session
        .input_queue
        .claim_pending_input_for_displaced_turn(&turn_state, &turn_context)
        .await
    else {
        panic!("the exact displaced turn should join its installed recording");
    };
    assert_eq!(joined.claimed_input(), Arc::clone(&items));
    let should_stop = false;
    completion.complete(should_stop).await;
    let completed = PendingInputRecordingResult::Completed { should_stop };
    assert_eq!(recording.wait().await, completed);
    assert_eq!(joined.wait().await, completed);

    let PendingInputClaim::Acquired(claim) = session
        .input_queue
        .claim_pending_input_for_displaced_turn(&turn_state, &turn_context)
        .await
    else {
        panic!("post-claim displaced input should become the next claim");
    };
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::ResponseItem(local_two)]);
    completion.complete(should_stop).await;
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_displaced_turn(&turn_state, &turn_context)
            .await,
        PendingInputClaim::Empty
    ));
    assert!(session.input_queue.has_pending_mailbox_items().await);

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
        panic!("the displaced turn should acquire its failure batch");
    };
    let failed_recording = claim.recording();
    let (items, completion) = claim.into_parts();
    assert_eq!(items.as_ref(), &[TurnInput::ResponseItem(failed.clone())]);
    drop(completion);
    assert_eq!(
        failed_recording.wait().await,
        PendingInputRecordingResult::Failed
    );
    assert!(matches!(
        session
            .input_queue
            .claim_pending_input_for_displaced_turn(&turn_state, &other_turn)
            .await,
        PendingInputClaim::Inactive
    ));
    let PendingInputClaim::Recording(retained) = session
        .input_queue
        .claim_pending_input_for_displaced_turn(&turn_state, &turn_context)
        .await
    else {
        panic!("the exact displaced turn should rejoin its sticky failed recording");
    };
    assert_eq!(
        retained.claimed_input().as_ref(),
        &[TurnInput::ResponseItem(failed)]
    );
    assert_eq!(retained.wait().await, PendingInputRecordingResult::Failed);

    session.abort_all_tasks(TurnAbortReason::Interrupted).await;
}

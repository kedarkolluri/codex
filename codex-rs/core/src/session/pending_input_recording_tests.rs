use super::*;
use crate::session::input_queue::PendingInputClaim;
use crate::session::input_queue::PendingInputRecordingResult;
use crate::state::TurnState;
use pretty_assertions::assert_eq;
use std::future::Future;
use std::future::pending;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Mutex as StdMutex;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

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

async fn active_turn_state(session: &Session) -> Arc<tokio::sync::Mutex<TurnState>> {
    let active = session.active_turn.lock().await;
    Arc::clone(
        &active
            .as_ref()
            .expect("test task should own the active turn")
            .turn_state,
    )
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
    inject(&session, &local).await;
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

async fn inject(session: &Session, item: &ResponseItem) {
    assert_eq!(session.inject_if_running(vec![item.clone()]).await, Ok(()));
}

async fn assert_recording_installed(session: &Session, turn_state: &tokio::sync::Mutex<TurnState>) {
    assert!(
        session
            .input_queue
            .pending_input_recording(turn_state)
            .await
            .is_some()
    );
}

async fn assert_pending_once<F: Future>(mut future: Pin<&mut F>) {
    poll_fn(move |cx| match future.as_mut().poll(cx) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("future completed before the synchronization barrier"),
    })
    .await;
}

#[derive(Debug, PartialEq)]
enum ReplacementEvent {
    CancellationObserved,
    ParentHardAborted,
    OldTaskAbort,
    SuccessorStarted(Vec<ResponseItem>),
}

struct ParentRunDrop {
    event_tx: mpsc::UnboundedSender<ReplacementEvent>,
}

impl Drop for ParentRunDrop {
    fn drop(&mut self) {
        let _ = self.event_tx.send(ReplacementEvent::ParentHardAborted);
    }
}

enum RaceTaskBehavior {
    HardAbort {
        started_tx: StdMutex<Option<oneshot::Sender<()>>>,
    },
    SnapshotHistory,
}

struct RaceTask {
    behavior: RaceTaskBehavior,
    event_tx: mpsc::UnboundedSender<ReplacementEvent>,
}

impl SessionTask for RaceTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.pending_input_recording_test"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        match &self.behavior {
            RaceTaskBehavior::HardAbort { started_tx } => {
                let _drop_signal = ParentRunDrop {
                    event_tx: self.event_tx.clone(),
                };
                started_tx
                    .lock()
                    .expect("started sender mutex should not be poisoned")
                    .take()
                    .expect("hard-abort task should start once")
                    .send(())
                    .expect("test should retain the started receiver");
                cancellation_token.cancelled().await;
                self.event_tx
                    .send(ReplacementEvent::CancellationObserved)
                    .expect("test should retain the event receiver");
                pending::<SessionTaskResult>().await
            }
            RaceTaskBehavior::SnapshotHistory => {
                let history = session
                    .clone_session()
                    .clone_history()
                    .await
                    .into_raw_items();
                self.event_tx
                    .send(ReplacementEvent::SuccessorStarted(history))
                    .expect("test should retain the event receiver");
                Ok(None)
            }
        }
    }

    async fn abort(&self, _session: Arc<SessionTaskContext>, _ctx: Arc<TurnContext>) {
        if matches!(&self.behavior, RaceTaskBehavior::HardAbort { .. }) {
            self.event_tx
                .send(ReplacementEvent::OldTaskAbort)
                .expect("test should retain the event receiver");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn hard_abort_preserves_post_claim_input_before_replacement_starts() {
    let (session, turn_context, _events_rx) = make_session_and_context_with_rx().await;
    let replacement_context = session
        .new_default_turn_with_sub_id("replacement-turn".to_string())
        .await;
    let baseline_len = strip_metadata_from_items(session.clone_history().await.raw_items()).len();
    let first = response_item("claimed before hard abort");
    let second = response_item("queued after the first claim");
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let (parent_started_tx, parent_started_rx) = oneshot::channel();

    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            RaceTask {
                behavior: RaceTaskBehavior::HardAbort {
                    started_tx: StdMutex::new(Some(parent_started_tx)),
                },
                event_tx: event_tx.clone(),
            },
        )
        .await;
    parent_started_rx
        .await
        .expect("hard-abort task should report that it started");
    let turn_state = active_turn_state(&session).await;

    inject(&session, &first).await;
    let history_guard = session.state.lock().await;
    let mut recording = Box::pin(
        session.record_pending_input_for_turn(&turn_context, PendingInputClaimMode::CurrentTurn),
    );
    assert_pending_once(recording.as_mut()).await;
    assert_recording_installed(&session, turn_state.as_ref()).await;
    inject(&session, &second).await;
    drop(recording);

    let replacement = tokio::spawn({
        let session = Arc::clone(&session);
        let event_tx = event_tx.clone();
        async move {
            session
                .spawn_task(
                    replacement_context,
                    Vec::new(),
                    RaceTask {
                        behavior: RaceTaskBehavior::SnapshotHistory,
                        event_tx,
                    },
                )
                .await;
        }
    });

    assert_eq!(
        event_rx
            .recv()
            .await
            .expect("old task should observe cancellation"),
        ReplacementEvent::CancellationObserved
    );
    tokio::time::advance(Duration::from_millis(101)).await;
    assert_eq!(
        event_rx
            .recv()
            .await
            .expect("old task run future should be hard-aborted"),
        ReplacementEvent::ParentHardAborted
    );
    assert!(
        event_rx.try_recv().is_err(),
        "old task abort and successor start must wait for the claimed recording"
    );

    drop(history_guard);
    replacement
        .await
        .expect("replacement orchestration task should complete");
    assert_eq!(
        event_rx
            .recv()
            .await
            .expect("old task abort should run after recording"),
        ReplacementEvent::OldTaskAbort
    );
    let successor_event = event_rx
        .recv()
        .await
        .expect("successor should observe committed history");
    let ReplacementEvent::SuccessorStarted(successor_history) = successor_event else {
        panic!("successor should start after old task abort: {successor_event:?}");
    };
    let relevant_history = strip_metadata_from_items(&successor_history)[baseline_len..].to_vec();
    assert_eq!(relevant_history, vec![first, second]);
}

#[tokio::test]
async fn natural_finish_remains_steerable_until_all_claimed_batches_commit() {
    let (session, turn_context, events_rx) = make_session_and_context_with_rx().await;
    let baseline_len = strip_metadata_from_items(session.clone_history().await.raw_items()).len();
    let first = response_item("claimed at natural finish");
    let second = response_item("queued during finalization recording");
    let mailbox = InterAgentCommunication::new(
        AgentPath::try_from("/root/worker").expect("worker path should parse"),
        AgentPath::root(),
        Vec::new(),
        "mail for the next turn".to_string(),
        /*trigger_turn*/ false,
    );
    session
        .spawn_task(
            Arc::clone(&turn_context),
            Vec::new(),
            NeverEndingTask {
                kind: TaskKind::Regular,
                listen_to_cancellation_token: false,
            },
        )
        .await;
    let turn_state = active_turn_state(&session).await;
    inject(&session, &first).await;
    session
        .input_queue
        .enqueue_mailbox_communication(mailbox.clone())
        .await;

    let history_guard = session.state.lock().await;
    let finalization = session.on_task_finished(Arc::clone(&turn_context), Ok(None));
    tokio::pin!(finalization);
    assert_pending_once(finalization.as_mut()).await;
    assert_recording_installed(&session, turn_state.as_ref()).await;
    inject(&session, &second).await;

    drop(history_guard);
    finalization.await;

    let completed = loop {
        let event = events_rx
            .recv()
            .await
            .expect("session event channel should remain open");
        if let EventMsg::TurnComplete(completed) = event.msg {
            break completed;
        }
    };
    assert_eq!(completed.turn_id, turn_context.sub_id);
    assert!(session.active_turn.lock().await.is_none());
    let history = strip_metadata_from_items(session.clone_history().await.raw_items());
    assert_eq!(history[baseline_len..].to_vec(), vec![first, second]);
    assert_eq!(
        session.input_queue.drain_mailbox_input_items().await,
        vec![TurnInput::InterAgentCommunication(mailbox)]
    );
}

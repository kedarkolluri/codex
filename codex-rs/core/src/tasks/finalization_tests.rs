use std::sync::Arc;
use std::time::Duration;

use codex_protocol::protocol::TurnAbortReason;
use std::task::Poll;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::PendingFinalization;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::SessionTaskResult;
use crate::tasks::TaskStartOutcome;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

struct WaitingTask;

impl SessionTask for WaitingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.finalization_probe"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

async fn start_waiting_task() -> (Arc<Session>, Arc<TurnContext>) {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    assert!(matches!(
        session
            .start_task(Arc::clone(&turn_context), Vec::new(), WaitingTask)
            .await,
        TaskStartOutcome::Started
    ));
    (session, turn_context)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropped_completion_wait_poisons_the_exact_finalization() {
    let (session, turn_context) = start_waiting_task().await;
    let generation = session
        .active_turn
        .lock()
        .await
        .running_generation()
        .expect("task should own a running generation");
    let finalizing_turn = session
        .active_turn
        .lock()
        .await
        .begin_finalization(&generation, &turn_context)
        .expect("exact running task should enter finalization");
    let (task, completion) = finalizing_turn.into_parts();
    task.handle.abort();
    let pending = PendingFinalization::new(&session, completion);

    let active_turn = session.active_turn.lock().await;
    let entered = Arc::new(Notify::new());
    let completion_task = {
        let entered = Arc::clone(&entered);
        tokio::spawn(async move {
            entered.notify_one();
            pending.complete().await
        })
    };
    timeout(TEST_TIMEOUT, entered.notified())
        .await
        .expect("completion attempt should reach the contended slot");
    completion_task.abort();
    let join_error = completion_task
        .await
        .expect_err("completion attempt should be cancelled");
    assert!(join_error.is_cancelled());
    drop(active_turn);

    timeout(TEST_TIMEOUT, generation.wait_lifecycle_finished())
        .await
        .expect("dropped completion authority should poison its exact finalization");
    let active_turn = session.active_turn.lock().await;
    assert!(active_turn.has_active_turn());
    assert!(active_turn.running_turn().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalization_drop_outside_runtime_context_uses_the_session_runtime() {
    let (session, turn_context) = start_waiting_task().await;
    let generation = session
        .active_turn
        .lock()
        .await
        .running_generation()
        .expect("task should own a running generation");
    let finalizing_turn = session
        .active_turn
        .lock()
        .await
        .begin_finalization(&generation, &turn_context)
        .expect("exact running task should enter finalization");
    let (task, completion) = finalizing_turn.into_parts();
    task.handle.abort();
    let pending = PendingFinalization::new(&session, completion);

    std::thread::spawn(move || drop(pending))
        .join()
        .expect("off-runtime drop thread should not panic");
    timeout(TEST_TIMEOUT, generation.wait_lifecycle_finished())
        .await
        .expect("session runtime should poison finalization after an off-runtime drop");
    let active_turn = session.active_turn.lock().await;
    assert!(active_turn.has_active_turn());
    assert!(active_turn.running_turn().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn abort_all_waits_for_an_existing_finalization() {
    let (session, turn_context) = start_waiting_task().await;
    let generation = session
        .active_turn
        .lock()
        .await
        .running_generation()
        .expect("task should own a running generation");
    let finalizing_turn = session
        .active_turn
        .lock()
        .await
        .begin_finalization(&generation, &turn_context)
        .expect("exact running task should enter finalization");
    let (task, completion) = finalizing_turn.into_parts();
    task.handle.abort();

    let abort = session.abort_all_tasks(TurnAbortReason::Interrupted);
    tokio::pin!(abort);
    assert!(matches!(futures::poll!(&mut abort), Poll::Pending));

    assert!(
        session
            .active_turn
            .lock()
            .await
            .complete_finalization(completion)
            .is_ok()
    );
    timeout(TEST_TIMEOUT, &mut abort)
        .await
        .expect("abort should resume after exact finalization");
    assert!(session.active_turn.lock().await.is_idle());
}

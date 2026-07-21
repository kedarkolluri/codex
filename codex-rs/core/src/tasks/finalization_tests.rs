use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use codex_protocol::protocol::TurnAbortReason;
use futures::task::noop_waker;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use super::PendingFinalization;
use super::PendingFinalizationOutcome;
use super::PendingFinalizationRecovery;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::RunningTask;
use crate::state::SessionTurnAbortTransition;
use crate::state::TaskKind;
use crate::state::turn_lifecycle::TurnGeneration;
use crate::tasks::AnySessionTask;
use crate::tasks::RegularTask;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);
const EXPECT_PENDING_TIMEOUT: Duration = Duration::from_millis(50);

fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    future.poll(&mut context)
}

fn running_task(turn_context: Arc<TurnContext>) -> RunningTask {
    let handle = tokio::spawn(std::future::pending::<()>());
    RunningTask {
        done: Arc::new(Notify::new()),
        handle: AbortOnDropHandle::new(handle),
        kind: TaskKind::Regular,
        task: Arc::new(RegularTask::new()) as Arc<dyn AnySessionTask>,
        cancellation_token: CancellationToken::new(),
        turn_context: Arc::clone(&turn_context),
        turn_extension_data: Arc::clone(&turn_context.extension_data),
        _agent_execution_guard: None,
        _timer: None,
    }
}

async fn begin_pending_finalization() -> (Arc<Session>, TurnGeneration, PendingFinalization) {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let Ok(driver) = session
        .active_turn
        .lock()
        .await
        .begin_fresh_start(/*execution_guard*/ None)
    else {
        panic!("idle session should accept an exact start");
    };
    let generation = driver.generation();
    assert!(
        session
            .active_turn
            .lock()
            .await
            .commit_start(driver, running_task(Arc::clone(&turn_context)))
            .is_ok()
    );
    let finalizing_turn = session
        .active_turn
        .lock()
        .await
        .begin_finalization(&generation, &turn_context)
        .expect("exact running turn should begin finalization");
    let (task, completion) = finalizing_turn.into_parts();
    task.handle.abort();
    let pending = PendingFinalization::new(Arc::clone(&session), completion);
    (session, generation, pending)
}

async fn wait_for_lifecycle(generation: &TurnGeneration, message: &str) {
    timeout(TEST_TIMEOUT, generation.wait_lifecycle_finished())
        .await
        .expect(message);
}

#[tokio::test]
async fn ordinary_completion_releases_the_exact_slot() {
    let (session, generation, pending) = begin_pending_finalization().await;
    assert!(Arc::ptr_eq(pending.turn_state(), generation.turn_state()));

    assert_eq!(
        pending.complete().await,
        PendingFinalizationOutcome::Completed
    );

    wait_for_lifecycle(
        &generation,
        "ordinary completion should finish the lifecycle",
    )
    .await;
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
}

#[tokio::test]
async fn abort_waits_for_an_existing_finalization_owner() {
    let (session, generation, pending) = begin_pending_finalization().await;
    let mut abort = tokio::spawn({
        let session = Arc::clone(&session);
        async move {
            session.abort_all_tasks(TurnAbortReason::Interrupted).await;
        }
    });
    assert!(timeout(EXPECT_PENDING_TIMEOUT, &mut abort).await.is_err());

    assert_eq!(
        pending.complete().await,
        PendingFinalizationOutcome::Completed
    );
    timeout(TEST_TIMEOUT, abort)
        .await
        .expect("abort should observe finalization completion")
        .expect("abort task should not panic");
    wait_for_lifecycle(&generation, "finalization should finish lifecycle").await;
}

#[tokio::test]
async fn explicit_poison_keeps_the_exact_slot_fail_closed() {
    let (session, generation, pending) = begin_pending_finalization().await;

    pending.poison().await;

    wait_for_lifecycle(&generation, "explicit poison should finish lifecycle").await;
    assert!(!session.active_turn.lock().await.can_begin_fresh_start());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_finalization_off_runtime_uses_the_session_runtime() {
    let (session, generation, pending) = begin_pending_finalization().await;

    std::thread::spawn(move || drop(pending))
        .join()
        .expect("off-runtime finalization drop should not panic");

    wait_for_lifecycle(
        &generation,
        "session runtime should poison the finalization",
    )
    .await;
    let mut active_turn = session.active_turn.lock().await;
    assert!(!active_turn.can_begin_fresh_start());
    assert!(matches!(
        active_turn.begin_abort(TurnAbortReason::Interrupted),
        SessionTurnAbortTransition::Inactive
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_completion_future_runs_detached_poison_recovery() {
    let (session, generation, pending) = begin_pending_finalization().await;
    {
        let _active_turn = session.active_turn.lock().await;
        let mut completion = Box::pin(pending.complete());
        assert!(matches!(poll_once(completion.as_mut()), Poll::Pending));
        drop(completion);
    }

    wait_for_lifecycle(
        &generation,
        "detached recovery should poison the finalization",
    )
    .await;
    let mut active_turn = session.active_turn.lock().await;
    assert!(!active_turn.can_begin_fresh_start());
    assert!(matches!(
        active_turn.begin_abort(TurnAbortReason::Interrupted),
        SessionTurnAbortTransition::Inactive
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_detached_recovery_uses_runtime_independent_poison_fallback() {
    let (session, generation, mut pending) = begin_pending_finalization().await;
    {
        let _active_turn = session.active_turn.lock().await;
        let recovery = PendingFinalizationRecovery {
            session: Arc::clone(&session),
            completion: pending.completion.take(),
        };
        drop(pending);
        let mut recovery = Box::pin(recovery.poison());
        assert!(matches!(poll_once(recovery.as_mut()), Poll::Pending));
        drop(recovery);
    }

    wait_for_lifecycle(&generation, "runtime-independent fallback should poison").await;
    let mut active_turn = session.active_turn.lock().await;
    assert!(!active_turn.can_begin_fresh_start());
    assert!(matches!(
        active_turn.begin_abort(TurnAbortReason::Interrupted),
        SessionTurnAbortTransition::Inactive
    ));
}

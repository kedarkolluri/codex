use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::stop_task_starts_for_shutdown;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::SessionTaskResult;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct PausingTurnStart {
    entered: Notify,
    release: Notify,
}

impl codex_extension_api::TurnLifecycleContributor for PausingTurnStart {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
        })
    }
}

struct PendingTask {
    ran: Arc<AtomicBool>,
    run_entered: Arc<Notify>,
}

impl SessionTask for PendingTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.shutdown_start_test"
    }

    async fn run(
        self: Arc<Self>,
        _session: Arc<SessionTaskContext>,
        _ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        self.ran.store(true, Ordering::SeqCst);
        self.run_entered.notify_one();
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

async fn make_session(probe: Arc<PausingTurnStart>) -> (Arc<Session>, Arc<TurnContext>) {
    let (mut session, turn_context) = make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.turn_lifecycle_contributor(probe);
    session.services.extensions = Arc::new(builder.build());
    (Arc::new(session), Arc::new(turn_context))
}

async fn wait_for(notify: &Notify, message: &str) {
    timeout(TEST_TIMEOUT, notify.notified())
        .await
        .expect(message);
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "the held turn-state lock is the deterministic commit barrier under test"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_in_commit_section_linearizes_before_shutdown_gate_close() {
    let probe = Arc::new(PausingTurnStart::default());
    let (session, turn_context) = make_session(Arc::clone(&probe)).await;
    let ran = Arc::new(AtomicBool::new(false));
    let run_entered = Arc::new(Notify::new());
    let task = PendingTask {
        ran: Arc::clone(&ran),
        run_entered: Arc::clone(&run_entered),
    };
    let session_for_start = Arc::clone(&session);
    let start = tokio::spawn(async move {
        session_for_start
            .spawn_task(turn_context, Vec::new(), task)
            .await;
    });
    wait_for(&probe.entered, "turn-start callback should be entered").await;

    let turn_state = {
        let active_turn = session.active_turn.lock().await;
        Arc::clone(
            active_turn
                .current_turn_state()
                .expect("starting turn should own state"),
        )
    };
    let turn_state_guard = turn_state.lock().await;
    probe.release.notify_one();
    timeout(TEST_TIMEOUT, async {
        loop {
            if session.active_turn.try_lock().is_err() {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("task start should reach its commit critical section");

    let shutdown_entered = Arc::new(Notify::new());
    let shutdown_entered_for_task = Arc::clone(&shutdown_entered);
    let session_for_shutdown = Arc::clone(&session);
    let shutdown = tokio::spawn(async move {
        shutdown_entered_for_task.notify_one();
        stop_task_starts_for_shutdown(session_for_shutdown.as_ref()).await;
    });
    wait_for(&shutdown_entered, "shutdown task should start").await;
    tokio::task::yield_now().await;
    assert!(session.turn_start_gate.is_open());

    drop(turn_state_guard);
    timeout(TEST_TIMEOUT, start)
        .await
        .expect("task start should finish")
        .expect("task start should not panic");
    timeout(TEST_TIMEOUT, shutdown)
        .await
        .expect("shutdown gate close should finish")
        .expect("shutdown gate close should not panic");
    assert!(!session.turn_start_gate.is_open());
    wait_for(&run_entered, "committed task should run").await;
    assert!(ran.load(Ordering::SeqCst));

    session
        .abort_all_tasks(codex_protocol::protocol::TurnAbortReason::Interrupted)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_gate_close_linearizes_before_inflight_start_commit() {
    let probe = Arc::new(PausingTurnStart::default());
    let (session, turn_context) = make_session(Arc::clone(&probe)).await;
    let ran = Arc::new(AtomicBool::new(false));
    let task = PendingTask {
        ran: Arc::clone(&ran),
        run_entered: Arc::new(Notify::new()),
    };
    let session_for_start = Arc::clone(&session);
    let start = tokio::spawn(async move {
        session_for_start
            .spawn_task(turn_context, Vec::new(), task)
            .await;
    });
    wait_for(&probe.entered, "turn-start callback should be entered").await;

    stop_task_starts_for_shutdown(session.as_ref()).await;
    assert!(!session.turn_start_gate.is_open());
    probe.release.notify_one();

    timeout(TEST_TIMEOUT, start)
        .await
        .expect("cancelled task start should finish")
        .expect("cancelled task start should not panic");
    assert!(!ran.load(Ordering::SeqCst));
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
}

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use super::TaskStartOutcome;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::state::turn_lifecycle::TurnStartOutcome as LifecycleStartOutcome;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::SessionTaskResult;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct StartGate {
    entered: Notify,
    release: Notify,
}

impl codex_extension_api::TurnLifecycleContributor for StartGate {
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

#[derive(Default)]
struct StartAbortGate {
    start_entered: Notify,
    start_release: Notify,
    abort_entered: Notify,
    abort_release: Notify,
    abort_completed: AtomicBool,
}

impl codex_extension_api::TurnLifecycleContributor for StartAbortGate {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.start_entered.notify_one();
            self.start_release.notified().await;
        })
    }

    fn on_turn_abort<'a>(
        &'a self,
        _input: codex_extension_api::TurnAbortInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.abort_entered.notify_one();
            self.abort_release.notified().await;
            self.abort_completed.store(true, Ordering::SeqCst);
        })
    }
}

struct StartCounter {
    calls: AtomicUsize,
}

impl codex_extension_api::TurnLifecycleContributor for StartCounter {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
        })
    }
}

#[derive(Default)]
struct TaskProbe {
    run_count: AtomicUsize,
    entered: Notify,
    observed_own_running_turn: AtomicBool,
}

struct ProbeTask {
    probe: Arc<TaskProbe>,
}

impl SessionTask for ProbeTask {
    fn kind(&self) -> TaskKind {
        TaskKind::Regular
    }

    fn span_name(&self) -> &'static str {
        "session_task.atomic_start_probe"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        let session = session.clone_session();
        let observed_own_running_turn = session
            .active_turn
            .lock()
            .await
            .running_turn()
            .is_some_and(|turn| Arc::ptr_eq(&turn.task().turn_context, &ctx));
        self.probe
            .observed_own_running_turn
            .store(observed_own_running_turn, Ordering::SeqCst);
        self.probe.run_count.fetch_add(1, Ordering::SeqCst);
        self.probe.entered.notify_one();
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

async fn make_gated_session() -> (Arc<Session>, Arc<TurnContext>, Arc<StartGate>) {
    let (mut session, turn_context) = make_session_and_context().await;
    let gate = Arc::new(StartGate::default());
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.turn_lifecycle_contributor(gate.clone());
    session.services.extensions = Arc::new(builder.build());
    (Arc::new(session), Arc::new(turn_context), gate)
}

async fn wait_for(notify: &Notify, message: &str) {
    timeout(TEST_TIMEOUT, notify.notified())
        .await
        .expect(message);
}

#[tokio::test]
async fn stale_reserved_start_cannot_consume_replacement_reservation() {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let probe = Arc::new(TaskProbe::default());

    let stale_turn_state = {
        let mut active_turn = session.active_turn.lock().await;
        Arc::clone(
            active_turn
                .reserve_taskless()
                .expect("idle slot should accept the original reservation"),
        )
    };
    let replacement_turn_state = {
        let mut active_turn = session.active_turn.lock().await;
        assert!(active_turn.clear_taskless_exact_state(&stale_turn_state));
        Arc::clone(
            active_turn
                .reserve_taskless()
                .expect("cleared slot should accept the replacement reservation"),
        )
    };

    let outcome = session
        .start_reserved_task(
            turn_context,
            Vec::new(),
            ProbeTask {
                probe: Arc::clone(&probe),
            },
            stale_turn_state,
        )
        .await;
    assert!(matches!(outcome, TaskStartOutcome::Busy));
    assert_eq!(0, probe.run_count.load(Ordering::SeqCst));

    let mut active_turn = session.active_turn.lock().await;
    assert!(active_turn.can_begin_reserved_start(&replacement_turn_state));
    assert!(active_turn.clear_taskless_exact_state(&replacement_turn_state));
    assert!(active_turn.is_idle());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_start_finishes_abort_lifecycle_before_publishing_outcome() {
    let (mut session, turn_context) = make_session_and_context().await;
    let gate = Arc::new(StartAbortGate::default());
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.turn_lifecycle_contributor(gate.clone());
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let probe = Arc::new(TaskProbe::default());
    let start = {
        let session = Arc::clone(&session);
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            session
                .start_task(Arc::new(turn_context), Vec::new(), ProbeTask { probe })
                .await
        })
    };
    wait_for(
        &gate.start_entered,
        "start should reach the lifecycle callback",
    )
    .await;

    let generation = session
        .active_turn
        .lock()
        .await
        .cancel_start(TurnAbortReason::Interrupted)
        .expect("gated task should still be Starting");
    gate.start_release.notify_one();
    wait_for(
        &gate.abort_entered,
        "cancelled start should enter its abort lifecycle callback",
    )
    .await;

    let generation_for_wait = generation.clone();
    let mut outcome_waiter = tokio::spawn(async move { generation_for_wait.wait_finished().await });
    assert!(
        timeout(Duration::from_millis(50), &mut outcome_waiter)
            .await
            .is_err(),
        "cancelled outcome must remain unpublished while abort lifecycle is blocked"
    );
    assert!(!start.is_finished());
    assert!(!session.active_turn.lock().await.is_idle());
    assert!(!gate.abort_completed.load(Ordering::SeqCst));

    gate.abort_release.notify_one();
    let outcome = timeout(TEST_TIMEOUT, start)
        .await
        .expect("start should finish after abort lifecycle completes")
        .expect("start task should not panic");
    assert!(matches!(
        outcome,
        TaskStartOutcome::Cancelled(TurnAbortReason::Interrupted)
    ));
    assert_eq!(
        LifecycleStartOutcome::Cancelled(TurnAbortReason::Interrupted),
        timeout(TEST_TIMEOUT, outcome_waiter)
            .await
            .expect("cancelled generation should publish its terminal outcome")
            .expect("outcome waiter should not panic")
    );
    assert!(gate.abort_completed.load(Ordering::SeqCst));
    assert_eq!(0, probe.run_count.load(Ordering::SeqCst));
    assert!(session.active_turn.lock().await.is_idle());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_before_start_lifecycle_skips_start_callback() {
    let (mut session, turn_context) = make_session_and_context().await;
    let start_counter = Arc::new(StartCounter {
        calls: AtomicUsize::new(0),
    });
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.turn_lifecycle_contributor(start_counter.clone());
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let probe = Arc::new(TaskProbe::default());

    let guardian_guard = session
        .services
        .guardian_rejection_circuit_breaker
        .lock()
        .await;
    let start = {
        let session = Arc::clone(&session);
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            session
                .start_task(Arc::new(turn_context), Vec::new(), ProbeTask { probe })
                .await
        })
    };
    let generation = timeout(TEST_TIMEOUT, async {
        loop {
            if let Some(generation) = session
                .active_turn
                .lock()
                .await
                .cancel_start(TurnAbortReason::Interrupted)
            {
                break generation;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("start should reserve the slot while guardian cleanup is blocked");
    drop(guardian_guard);

    let outcome = timeout(TEST_TIMEOUT, start)
        .await
        .expect("cancelled start should finish after guardian cleanup resumes")
        .expect("start task should not panic");
    assert!(matches!(
        outcome,
        TaskStartOutcome::Cancelled(TurnAbortReason::Interrupted)
    ));
    assert_eq!(
        LifecycleStartOutcome::Cancelled(TurnAbortReason::Interrupted),
        timeout(TEST_TIMEOUT, generation.wait_finished())
            .await
            .expect("cancelled generation should publish its terminal outcome")
    );
    assert_eq!(0, start_counter.calls.load(Ordering::SeqCst));
    assert_eq!(0, probe.run_count.load(Ordering::SeqCst));
    assert!(session.active_turn.lock().await.is_idle());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_start_lifecycle_skips_later_contributors() {
    let (mut session, turn_context) = make_session_and_context().await;
    let first = Arc::new(StartGate::default());
    let later = Arc::new(StartCounter {
        calls: AtomicUsize::new(0),
    });
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    builder.turn_lifecycle_contributor(first.clone());
    builder.turn_lifecycle_contributor(later.clone());
    session.services.extensions = Arc::new(builder.build());
    let session = Arc::new(session);
    let probe = Arc::new(TaskProbe::default());
    let start = {
        let session = Arc::clone(&session);
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            session
                .start_task(Arc::new(turn_context), Vec::new(), ProbeTask { probe })
                .await
        })
    };
    wait_for(
        &first.entered,
        "first contributor should receive the start callback",
    )
    .await;
    let generation = session
        .active_turn
        .lock()
        .await
        .cancel_start(TurnAbortReason::Interrupted)
        .expect("start should remain cancellable while the first contributor is blocked");
    first.release.notify_one();

    let outcome = timeout(TEST_TIMEOUT, start)
        .await
        .expect("cancelled start should finish")
        .expect("start task should not panic");
    assert!(matches!(
        outcome,
        TaskStartOutcome::Cancelled(TurnAbortReason::Interrupted)
    ));
    assert_eq!(
        LifecycleStartOutcome::Cancelled(TurnAbortReason::Interrupted),
        generation.wait_finished().await
    );
    assert_eq!(0, later.calls.load(Ordering::SeqCst));
    assert_eq!(0, probe.run_count.load(Ordering::SeqCst));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_start_admits_exactly_one_task() {
    let (session, turn_context, gate) = make_gated_session().await;
    let probe = Arc::new(TaskProbe::default());

    let first_start = {
        let session = Arc::clone(&session);
        let turn_context = Arc::clone(&turn_context);
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            session
                .start_task(turn_context, Vec::new(), ProbeTask { probe })
                .await
        })
    };
    wait_for(&gate.entered, "first start should reach the lifecycle gate").await;

    let second_outcome = timeout(
        TEST_TIMEOUT,
        session.start_task(
            Arc::clone(&turn_context),
            Vec::new(),
            ProbeTask {
                probe: Arc::clone(&probe),
            },
        ),
    )
    .await
    .expect("competing start should resolve while the first start is gated");
    assert!(matches!(second_outcome, TaskStartOutcome::Busy));

    gate.release.notify_one();
    let first_outcome = timeout(TEST_TIMEOUT, first_start)
        .await
        .expect("first start should finish after its lifecycle gate opens")
        .expect("first start task should not panic");
    assert!(matches!(first_outcome, TaskStartOutcome::Started));
    wait_for(&probe.entered, "the admitted task should begin running").await;
    assert_eq!(1, probe.run_count.load(Ordering::SeqCst));

    session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    assert!(session.active_turn.lock().await.is_idle());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_starting_turn_prevents_task_body_and_restores_idle() {
    let (session, turn_context, gate) = make_gated_session().await;
    let probe = Arc::new(TaskProbe::default());
    let start = {
        let session = Arc::clone(&session);
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            session
                .start_task(turn_context, Vec::new(), ProbeTask { probe })
                .await
        })
    };
    wait_for(&gate.entered, "start should reach the lifecycle gate").await;

    let generation = session
        .active_turn
        .lock()
        .await
        .cancel_start(TurnAbortReason::Interrupted)
        .expect("gated task should still be Starting");
    gate.release.notify_one();

    let outcome = timeout(TEST_TIMEOUT, start)
        .await
        .expect("cancelled start should finish")
        .expect("start task should not panic");
    assert!(matches!(
        outcome,
        TaskStartOutcome::Cancelled(TurnAbortReason::Interrupted)
    ));
    assert_eq!(
        LifecycleStartOutcome::Cancelled(TurnAbortReason::Interrupted),
        timeout(TEST_TIMEOUT, generation.wait_finished())
            .await
            .expect("cancelled generation should publish its terminal outcome")
    );
    assert_eq!(0, probe.run_count.load(Ordering::SeqCst));
    assert!(session.active_turn.lock().await.is_idle());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_start_future_resolves_generation_without_running_task() {
    let (session, turn_context, gate) = make_gated_session().await;
    let probe = Arc::new(TaskProbe::default());
    let start = {
        let session = Arc::clone(&session);
        let probe = Arc::clone(&probe);
        tokio::spawn(async move {
            session
                .start_task(turn_context, Vec::new(), ProbeTask { probe })
                .await
        })
    };
    wait_for(&gate.entered, "start should reach the lifecycle gate").await;

    let generation = session
        .active_turn
        .lock()
        .await
        .current_generation()
        .expect("gated task should still own its Starting generation");
    start.abort();
    let join_result = timeout(TEST_TIMEOUT, start)
        .await
        .expect("aborted start future should be dropped promptly");
    match join_result {
        Err(err) => assert!(err.is_cancelled()),
        Ok(_) => panic!("start future unexpectedly completed before it was dropped"),
    }

    let lifecycle_outcome = timeout(TEST_TIMEOUT, generation.wait_finished())
        .await
        .expect("dropped start must compensate or poison its generation");
    assert!(matches!(
        lifecycle_outcome,
        LifecycleStartOutcome::Cancelled(TurnAbortReason::Interrupted)
            | LifecycleStartOutcome::Poisoned(TurnAbortReason::Interrupted)
    ));
    assert_eq!(0, probe.run_count.load(Ordering::SeqCst));
    assert!(session.active_turn.lock().await.running_turn().is_none());

    gate.release.notify_one();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_body_first_observes_its_committed_running_turn() {
    let (session, turn_context) = make_session_and_context().await;
    let session = Arc::new(session);
    let turn_context = Arc::new(turn_context);
    let probe = Arc::new(TaskProbe::default());

    let outcome = session
        .start_task(
            turn_context,
            Vec::new(),
            ProbeTask {
                probe: Arc::clone(&probe),
            },
        )
        .await;
    assert!(matches!(outcome, TaskStartOutcome::Started));
    wait_for(&probe.entered, "committed task should begin running").await;

    assert!(probe.observed_own_running_turn.load(Ordering::SeqCst));
    assert_eq!(1, probe.run_count.load(Ordering::SeqCst));

    session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    assert!(session.active_turn.lock().await.is_idle());
}

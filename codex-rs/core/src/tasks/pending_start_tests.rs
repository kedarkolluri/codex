use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::TokenUsage;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use tokio::sync::Notify;
use tokio::time::timeout;

use super::PendingTaskStart;
use super::PendingTaskStartOutcome;
use super::PendingTaskStartRecovery;
use crate::tasks::RegularTask;
use crate::tasks::TaskStartOutcome;
use crate::session::session::Session;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::SessionTurnAbortTransition;
use crate::state::turn_lifecycle::TurnGeneration;
use crate::state::turn_lifecycle::TurnStartOutcome;

const TEST_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Default)]
struct LifecycleProbe {
    block_start: bool,
    block_first_abort: bool,
    block_all_aborts: bool,
    start_calls: AtomicUsize,
    abort_calls: AtomicUsize,
    abort_completions: AtomicUsize,
    start_entered: Notify,
    start_release: Notify,
    abort_entered: Notify,
}

impl codex_extension_api::TurnLifecycleContributor for LifecycleProbe {
    fn on_turn_start<'a>(
        &'a self,
        _input: codex_extension_api::TurnStartInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            self.start_calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
            if self.block_start {
                self.start_entered.notify_one();
                self.start_release.notified().await;
            }
        })
    }

    fn on_turn_abort<'a>(
        &'a self,
        _input: codex_extension_api::TurnAbortInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let call = self.abort_calls.fetch_add(/*val*/ 1, Ordering::SeqCst);
            if self.block_all_aborts || (self.block_first_abort && call == 0) {
                self.abort_entered.notify_one();
                std::future::pending::<()>().await;
            }
            self.abort_completions
                .fetch_add(/*val*/ 1, Ordering::SeqCst);
        })
    }
}

impl LifecycleProbe {
    fn snapshot(&self) -> (usize, usize, usize) {
        (
            self.start_calls.load(Ordering::SeqCst),
            self.abort_calls.load(Ordering::SeqCst),
            self.abort_completions.load(Ordering::SeqCst),
        )
    }
}

async fn make_session(probes: &[Arc<LifecycleProbe>]) -> (Arc<Session>, Arc<TurnContext>) {
    let (mut session, turn_context) = make_session_and_context().await;
    let mut builder = codex_extension_api::ExtensionRegistryBuilder::<crate::config::Config>::new();
    for probe in probes {
        builder.turn_lifecycle_contributor(probe.clone());
    }
    session.services.extensions = Arc::new(builder.build());
    (Arc::new(session), Arc::new(turn_context))
}

async fn begin_pending_start(
    session: &Arc<Session>,
    turn_context: &Arc<TurnContext>,
) -> PendingTaskStart {
    let Ok(driver) = session
        .active_turn
        .lock()
        .await
        .begin_fresh_start(/*execution_guard*/ None)
    else {
        panic!("idle session should accept an exact task start");
    };
    PendingTaskStart::new(Arc::clone(session), Arc::clone(turn_context), driver)
}

async fn enter_start_lifecycle(
    session: &Session,
    turn_context: &TurnContext,
    pending_start: &mut PendingTaskStart,
) {
    let generation = pending_start.generation();
    session
        .emit_cancellable_turn_start_lifecycle(
            turn_context,
            &TokenUsage::default(),
            &generation,
            pending_start.lifecycle_progress_mut(),
        )
        .await;
}

async fn wait_for(notify: &Notify, message: &str) {
    timeout(TEST_TIMEOUT, notify.notified())
        .await
        .expect(message);
}

async fn abort_task<T>(task: tokio::task::JoinHandle<T>, message: &str) {
    task.abort();
    let Err(error) = timeout(TEST_TIMEOUT, task).await.expect(message) else {
        panic!("{message}");
    };
    assert!(error.is_cancelled());
}

async fn wait_for_outcome(generation: &TurnGeneration, message: &str) -> TurnStartOutcome {
    timeout(TEST_TIMEOUT, generation.wait_finished())
        .await
        .expect(message)
}

#[tokio::test]
async fn ordinary_compensation_aborts_entered_callbacks_and_restores_idle() {
    let probe = Arc::new(LifecycleProbe::default());
    let (session, turn_context) = make_session(&[Arc::clone(&probe)]).await;
    let mut pending_start = begin_pending_start(&session, &turn_context).await;
    let generation = pending_start.generation();
    enter_start_lifecycle(&session, &turn_context, &mut pending_start).await;
    assert!(
        session
            .active_turn
            .lock()
            .await
            .cancel_start_exact(&generation, TurnAbortReason::Replaced)
    );

    assert_eq!(
        pending_start.compensate().await,
        PendingTaskStartOutcome::Cancelled(TurnAbortReason::Replaced)
    );
    assert_eq!(
        wait_for_outcome(&generation, "ordinary compensation should finish").await,
        TurnStartOutcome::Cancelled(TurnAbortReason::Replaced)
    );
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
    assert_eq!(probe.snapshot(), (1, 1, 1));
}

#[tokio::test]
async fn explicit_poison_aborts_entered_callbacks_and_keeps_slot_closed() {
    let probe = Arc::new(LifecycleProbe::default());
    let (session, turn_context) = make_session(&[Arc::clone(&probe)]).await;
    let mut pending_start = begin_pending_start(&session, &turn_context).await;
    let generation = pending_start.generation();
    enter_start_lifecycle(&session, &turn_context, &mut pending_start).await;

    assert_eq!(
        pending_start.poison().await,
        PendingTaskStartOutcome::Poisoned
    );
    assert_eq!(
        wait_for_outcome(&generation, "poisoned start should finish").await,
        TurnStartOutcome::Poisoned(TurnAbortReason::Interrupted)
    );
    assert!(!session.active_turn.lock().await.can_begin_fresh_start());
    assert_eq!(probe.snapshot(), (1, 1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_abort_cancels_a_real_starting_task_and_restores_idle() {
    let probe = Arc::new(LifecycleProbe {
        block_start: true,
        ..Default::default()
    });
    let (session, turn_context) = make_session(&[Arc::clone(&probe)]).await;
    let session_for_start = Arc::clone(&session);
    let start = tokio::spawn(async move {
        let admission_permit = session_for_start
            .turn_start_gate
            .acquire_start_permit()
            .await;
        session_for_start
            .start_task_with_admission_permit(
                turn_context,
                Vec::new(),
                RegularTask::new(),
                admission_permit,
            )
            .await
    });
    wait_for(
        &probe.start_entered,
        "start callback should be entered before replacement",
    )
    .await;

    session.abort_all_tasks(TurnAbortReason::Replaced).await;

    let outcome = timeout(TEST_TIMEOUT, start)
        .await
        .expect("starting task should finish cancellation")
        .expect("starting task should not panic");
    assert!(matches!(
        outcome,
        TaskStartOutcome::Cancelled(TurnAbortReason::Replaced)
    ));
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
    assert_eq!(probe.snapshot(), (1, 1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_pending_work_caller_does_not_strand_trigger_mail() {
    let probe = Arc::new(LifecycleProbe {
        block_start: true,
        ..Default::default()
    });
    let (session, _turn_context) = make_session(&[Arc::clone(&probe)]).await;
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("worker path should parse"),
            AgentPath::root(),
            Vec::new(),
            "queued update".to_string(),
            /*trigger_turn*/ true,
        ))
        .await;
    let pending_work = tokio::spawn(
        session.maybe_start_turn_for_pending_work_with_sub_id("cancelled-caller".to_string()),
    );
    wait_for(
        &probe.start_entered,
        "pending-work start callback should be entered before caller cancellation",
    )
    .await;

    abort_task(
        pending_work,
        "pending-work caller should be independently cancellable",
    )
    .await;
    probe.start_release.notify_one();
    timeout(TEST_TIMEOUT, async {
        while session.input_queue.has_trigger_turn_mailbox_items().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("detached pending-work driver should attach queued trigger mail");

    assert!(session.active_turn.lock().await.has_active_turn());
    session.abort_all_tasks(TurnAbortReason::Replaced).await;
}

#[tokio::test]
async fn suppressed_completion_ticket_leaves_trigger_mail_queued() {
    let (session, _turn_context) = make_session(&[]).await;
    session
        .input_queue
        .enqueue_mailbox_communication(InterAgentCommunication::new(
            AgentPath::try_from("/root/worker").expect("worker path should parse"),
            AgentPath::root(),
            Vec::new(),
            "queued update".to_string(),
            /*trigger_turn*/ true,
        ))
        .await;
    let stale_ticket = session
        .turn_start_gate
        .automatic_start_ticket()
        .expect("gate open");
    session.turn_start_gate.suppress_automatic_starts();

    session
        .maybe_start_turn_for_pending_work_with_ticket(stale_ticket)
        .await;

    assert!(session.input_queue.has_trigger_turn_mailbox_items().await);
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_the_start_future_runs_detached_compensation() {
    let probe = Arc::new(LifecycleProbe {
        block_start: true,
        ..Default::default()
    });
    let (session, turn_context) = make_session(&[Arc::clone(&probe)]).await;
    let pending_start = begin_pending_start(&session, &turn_context).await;
    let generation = pending_start.generation();
    let session_for_start = Arc::clone(&session);
    let turn_context_for_start = Arc::clone(&turn_context);
    let start = tokio::spawn(async move {
        let mut pending_start = pending_start;
        enter_start_lifecycle(
            session_for_start.as_ref(),
            turn_context_for_start.as_ref(),
            &mut pending_start,
        )
        .await;
        pending_start.compensate().await
    });
    wait_for(
        &probe.start_entered,
        "start callback should be entered before cancellation",
    )
    .await;

    abort_task(start, "start future should be cancelled").await;
    assert_eq!(
        wait_for_outcome(&generation, "detached compensation should finish").await,
        TurnStartOutcome::Cancelled(TurnAbortReason::Interrupted)
    );
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
    assert_eq!(probe.snapshot(), (1, 1, 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detached_compensation_resumes_without_replaying_completed_callbacks() {
    let first = Arc::new(LifecycleProbe::default());
    let second = Arc::new(LifecycleProbe {
        block_first_abort: true,
        ..Default::default()
    });
    let third = Arc::new(LifecycleProbe::default());
    let probes = [Arc::clone(&first), Arc::clone(&second), Arc::clone(&third)];
    let (session, turn_context) = make_session(&probes).await;
    let mut pending_start = begin_pending_start(&session, &turn_context).await;
    let generation = pending_start.generation();
    enter_start_lifecycle(&session, &turn_context, &mut pending_start).await;
    assert!(
        session
            .active_turn
            .lock()
            .await
            .cancel_start_exact(&generation, TurnAbortReason::Interrupted)
    );
    let compensation = tokio::spawn(pending_start.compensate());
    wait_for(
        &second.abort_entered,
        "second abort callback should block foreground compensation",
    )
    .await;

    abort_task(compensation, "foreground compensation should be cancelled").await;
    assert_eq!(
        wait_for_outcome(&generation, "resumed compensation should finish").await,
        TurnStartOutcome::Cancelled(TurnAbortReason::Interrupted)
    );
    assert!(session.active_turn.lock().await.can_begin_fresh_start());
    assert_eq!(
        probes.map(|probe| probe.snapshot()),
        [(1, 1, 1), (1, 2, 1), (1, 1, 1)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_detached_recovery_poison_terminalizes_without_more_callbacks() {
    let first = Arc::new(LifecycleProbe::default());
    let second = Arc::new(LifecycleProbe {
        block_all_aborts: true,
        ..Default::default()
    });
    let third = Arc::new(LifecycleProbe::default());
    let probes = [Arc::clone(&first), Arc::clone(&second), Arc::clone(&third)];
    let (session, turn_context) = make_session(&probes).await;
    let mut pending_start = begin_pending_start(&session, &turn_context).await;
    let generation = pending_start.generation();
    enter_start_lifecycle(&session, &turn_context, &mut pending_start).await;
    let recovery = PendingTaskStartRecovery {
        session: Arc::clone(&session),
        turn_context: Arc::clone(&turn_context),
        recovery_state: pending_start.recovery_state.take(),
    };
    drop(pending_start);
    let recovery = tokio::spawn(recovery.recover());
    wait_for(
        &second.abort_entered,
        "second abort callback should block detached recovery",
    )
    .await;

    abort_task(recovery, "detached recovery should be cancelled").await;
    assert_eq!(
        wait_for_outcome(&generation, "poison fallback should finish").await,
        TurnStartOutcome::Poisoned(TurnAbortReason::Interrupted)
    );
    timeout(TEST_TIMEOUT, generation.wait_lifecycle_finished())
        .await
        .expect("poison fallback should finish the lifecycle");
    let mut active_turn = session.active_turn.lock().await;
    assert!(!active_turn.can_begin_fresh_start());
    assert!(matches!(
        active_turn.begin_abort(TurnAbortReason::Interrupted),
        SessionTurnAbortTransition::Inactive
    ));
    drop(active_turn);
    assert_eq!(
        probes.map(|probe| probe.snapshot()),
        [(1, 1, 1), (1, 1, 0), (1, 0, 0)]
    );
}

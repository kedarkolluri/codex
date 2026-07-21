use std::sync::Arc;

use pretty_assertions::assert_eq;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;
use tokio_util::task::AbortOnDropHandle;

use super::*;
use crate::session::tests::make_session_and_context;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::state::turn_lifecycle::TurnStartOutcome;
use crate::tasks::AnySessionTask;
use crate::tasks::RegularTask;

pub(super) fn running_task(turn_context: Arc<TurnContext>) -> RunningTask {
    let handle = tokio::spawn(std::future::pending::<()>());
    RunningTask {
        done: Arc::new(Notify::new()),
        kind: TaskKind::Regular,
        task: Arc::new(RegularTask::new()) as Arc<dyn AnySessionTask>,
        cancellation_token: CancellationToken::new(),
        handle: AbortOnDropHandle::new(handle),
        turn_extension_data: Arc::clone(&turn_context.extension_data),
        turn_context,
        _agent_execution_guard: None,
        _timer: None,
    }
}

#[tokio::test]
async fn taskless_reservation_keeps_exact_state_until_cleanup_or_abort() {
    let mut slot = SessionTurnSlot::default();
    let turn_state = Arc::clone(slot.reserve_taskless().expect("idle slot should reserve"));
    let repeated = slot
        .reserve_taskless()
        .expect("taskless reservation should be stable");

    assert!(Arc::ptr_eq(repeated, &turn_state));
    assert!(slot.has_active_turn());
    assert!(!slot.clear_taskless_exact_state(&Arc::new(Mutex::new(TurnState::default()))));
    assert!(slot.clear_taskless_exact_state(&turn_state));
    assert!(slot.is_idle());

    let turn_state = Arc::clone(slot.reserve_taskless().expect("idle slot should reserve"));
    let active_turn = slot
        .take_for_legacy_abort()
        .expect("taskless reservation should be removable");
    assert!(active_turn.running_task().is_none());
    assert!(Arc::ptr_eq(active_turn.turn_state(), &turn_state));
    assert!(slot.is_idle());
}

#[tokio::test]
async fn finish_projection_retains_state_until_exact_legacy_cleanup() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = SessionTurnSlot::default();
    let turn_state = Arc::clone(slot.reserve_taskless_for_legacy_start());
    slot.install_running_task_for_legacy_start(&turn_state, running_task(turn_context));

    let (_task, finished_state) = slot
        .take_running_task_for_legacy_finish()
        .expect("running task should project into finalization");
    assert!(Arc::ptr_eq(&finished_state, &turn_state));
    assert!(slot.running_turn().is_none());
    assert!(slot.has_active_turn());
    assert!(Arc::ptr_eq(
        slot.reserve_taskless()
            .expect("legacy taskless view should remain available"),
        &turn_state,
    ));
    assert!(!slot.clear_legacy_finished_exact_state(&Arc::new(Mutex::new(TurnState::default(),))));
    assert!(slot.clear_legacy_finished_exact_state(&turn_state));
    assert!(slot.is_idle());
}

#[tokio::test]
async fn finish_bridge_authenticates_the_running_context() {
    let (session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let turn_id = turn_context.sub_id.clone();
    let lookalike_context = Arc::new(session.new_default_turn_with_sub_id(turn_id.clone()).await);
    let mut slot = SessionTurnSlot::default();
    let turn_state = Arc::clone(slot.reserve_taskless_for_legacy_start());
    slot.install_running_task_for_legacy_start(
        &turn_state,
        running_task(Arc::clone(&turn_context)),
    );

    assert!(
        slot.begin_running_finalization_for_turn("other-turn")
            .is_none()
    );
    assert!(
        slot.begin_running_finalization_for_context(&lookalike_context)
            .is_none()
    );
    let finalizing = slot
        .begin_running_finalization_for_context(&turn_context)
        .expect("the exact running context should own finalization");
    let (task, completion) = finalizing.into_parts();
    assert_eq!(task.turn_context.sub_id, turn_id);
    assert!(Arc::ptr_eq(completion.turn_state(), &turn_state));
    task.handle.abort();
    assert!(slot.complete_finalization(completion).is_ok());
    assert!(slot.is_idle());
}

#[tokio::test]
async fn cancelled_legacy_start_cannot_install_into_a_successor_reservation() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = SessionTurnSlot::default();
    let stale_state = Arc::clone(slot.reserve_taskless_for_legacy_start());
    let SessionTurnAbortTransition::Starting(cancelled) =
        slot.begin_abort(TurnAbortReason::Replaced)
    else {
        panic!("stored legacy reservation should cancel exactly");
    };
    assert_eq!(
        cancelled.wait_finished().await,
        TurnStartOutcome::Cancelled(TurnAbortReason::Replaced)
    );
    assert!(slot.is_idle());

    let successor_state = Arc::clone(slot.reserve_taskless_for_legacy_start());
    assert!(!Arc::ptr_eq(&successor_state, &stale_state));
    slot.install_running_task_for_legacy_start(
        &stale_state,
        running_task(Arc::clone(&turn_context)),
    );
    assert!(slot.can_begin_reserved_start(&successor_state));
    slot.install_running_task_for_legacy_start(
        &successor_state,
        running_task(Arc::clone(&turn_context)),
    );

    let SessionTurnAbortTransition::Running(finalizing) =
        slot.begin_abort(TurnAbortReason::Interrupted)
    else {
        panic!("compatibility running turn should transfer into finalization");
    };
    let (task, completion) = finalizing.into_parts();
    task.handle.abort();
    assert!(matches!(
        slot.begin_abort(TurnAbortReason::Interrupted),
        SessionTurnAbortTransition::Finalizing(_)
    ));
    assert!(slot.complete_finalization(completion).is_ok());
    assert!(slot.is_idle());
}

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
    slot.install_running_task_for_legacy_start(running_task(turn_context));

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
async fn legacy_start_reopens_taskless_finalization_without_losing_state() {
    let (session, first_context) = make_session_and_context().await;
    let second_context = session
        .new_default_turn_with_sub_id("replacement-turn".to_string())
        .await;
    let first_context = Arc::new(first_context);
    let second_context = Arc::new(second_context);
    let mut slot = SessionTurnSlot::default();
    let turn_state = Arc::clone(slot.reserve_taskless_for_legacy_start());
    slot.install_running_task_for_legacy_start(running_task(first_context));
    let (_first_task, finished_state) = slot
        .take_running_task_for_legacy_finish()
        .expect("first task should project into finalization");

    assert!(Arc::ptr_eq(
        slot.reserve_taskless_for_legacy_start(),
        &finished_state,
    ));
    slot.install_running_task_for_legacy_start(running_task(Arc::clone(&second_context)));

    let running = slot.running_turn().expect("replacement should be running");
    assert_eq!(running.task().turn_context.sub_id, second_context.sub_id);
    assert!(Arc::ptr_eq(running.turn_state(), &turn_state));
    assert!(!slot.clear_legacy_finished_exact_state(&finished_state));
    assert!(
        slot.take_running_turn_for_abort(&second_context.sub_id)
            .is_some()
    );
    assert!(slot.is_idle());
}

#[tokio::test]
async fn targeted_abort_removes_only_the_matching_running_turn() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let turn_id = turn_context.sub_id.clone();
    let mut slot = SessionTurnSlot::default();
    let turn_state = Arc::clone(slot.reserve_taskless_for_legacy_start());
    slot.install_running_task_for_legacy_start(running_task(turn_context));

    assert!(slot.take_running_turn_for_abort("other-turn").is_none());
    assert!(slot.begin_running_finalization_for_turn(&turn_id).is_none());
    let active_turn = slot
        .take_running_turn_for_abort(&turn_id)
        .expect("matching running turn should be removed");
    assert_eq!(
        active_turn
            .running_task()
            .map(|task| task.turn_context.sub_id.as_str()),
        Some(turn_id.as_str()),
    );
    assert!(Arc::ptr_eq(active_turn.turn_state(), &turn_state));
    assert!(slot.is_idle());
}

#[tokio::test]
async fn legacy_abort_between_start_phases_and_during_finalization_restores_idle() {
    let (_session, turn_context) = make_session_and_context().await;
    let mut slot = SessionTurnSlot::default();
    let stale_state = Arc::clone(slot.reserve_taskless_for_legacy_start());
    assert!(slot.take_for_legacy_abort().is_some());

    slot.install_running_task_for_legacy_start(running_task(Arc::new(turn_context)));
    let running_state = Arc::clone(
        slot.running_turn()
            .expect("replacement running")
            .turn_state(),
    );
    assert!(!Arc::ptr_eq(&running_state, &stale_state));
    let (_task, finalizing_state) = slot
        .take_running_task_for_legacy_finish()
        .expect("replacement should enter finalization");
    assert!(slot.take_for_legacy_abort().is_some());
    assert!(slot.is_idle());
    assert!(!slot.clear_legacy_finished_exact_state(&finalizing_state));
}

use std::sync::Arc;
use std::task::Poll;
use std::time::Duration;

use codex_protocol::SessionId;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::TurnAbortReason;
use pretty_assertions::assert_eq;
use tokio::sync::Mutex;
use tokio::time::timeout;

use super::*;
use crate::agent::control::AgentControl;
use crate::agent::control::AgentExecutionAdmission;
use crate::agent::control::AgentExecutionGuard;
use crate::session::tests::make_session_and_context;
use crate::state::TurnState;
use crate::state::session_turn_slot::tests::running_task;
use crate::state::turn_lifecycle::TurnStartOutcome;

fn limited_source() -> SessionSource {
    SessionSource::SubAgent(SubAgentSource::Other("worker".to_string()))
}

fn admit_guard(control: &AgentControl, source: &SessionSource) -> AgentExecutionGuard {
    let AgentExecutionAdmission::Admitted(guard) =
        control.execution_admission(MultiAgentVersion::V2, source)
    else {
        panic!("limited turn should receive the available execution guard");
    };
    guard
}

fn assert_at_capacity(control: &AgentControl, source: &SessionSource) {
    assert!(matches!(
        control.execution_admission(MultiAgentVersion::V2, source),
        AgentExecutionAdmission::AtCapacity(_)
    ));
}

#[tokio::test]
async fn exact_reserved_start_authenticates_and_preserves_state() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = SessionTurnSlot::default();
    let reserved_state = Arc::clone(slot.reserve_taskless().expect("idle slot should reserve"));
    let foreign_state = Arc::new(Mutex::new(TurnState::default()));

    let Err(None) = slot.begin_reserved_start(&foreign_state, /*execution_guard*/ None) else {
        panic!("foreign state must not take the reserved driver");
    };
    assert!(slot.can_begin_reserved_start(&reserved_state));
    let Ok(driver) = slot.begin_reserved_start(&reserved_state, /*execution_guard*/ None) else {
        panic!("exact reserved state should transfer its driver");
    };
    let generation = driver.generation();
    assert!(Arc::ptr_eq(generation.turn_state(), &reserved_state));
    assert!(
        slot.commit_start(driver, running_task(Arc::clone(&turn_context)))
            .is_ok()
    );
    assert!(Arc::ptr_eq(
        slot.running_turn()
            .expect("exact task should be running")
            .turn_state(),
        &reserved_state,
    ));
}

#[tokio::test]
async fn execution_guard_is_held_by_the_exact_lifecycle_until_completion() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let control = AgentControl::default().with_session_id(SessionId::new(), /*max_threads*/ 1);
    let source = limited_source();
    let guard = admit_guard(&control, &source);
    let mut slot = SessionTurnSlot::default();
    let Ok(driver) = slot.begin_fresh_start(Some(guard)) else {
        panic!("idle slot should accept exact execution capacity");
    };
    let generation = driver.generation();
    assert_at_capacity(&control, &source);
    let mut task = running_task(Arc::clone(&turn_context));
    task._agent_execution_guard = control.execution_guard(MultiAgentVersion::V2, &source);
    let Err((driver, mut task)) = slot.commit_start(driver, task) else {
        panic!("exact commit must reject a second task-owned execution guard");
    };
    drop(task._agent_execution_guard.take());
    assert!(slot.commit_start(driver, task).is_ok());
    assert!(slot.take_running_task_for_legacy_finish().is_none());
    assert!(slot.take_for_legacy_abort().is_none());
    assert!(
        slot.begin_running_finalization_for_turn("foreign-turn")
            .is_none()
    );
    let finalizing = slot
        .begin_running_finalization_for_turn(&turn_context.sub_id)
        .expect("exact owner should begin finalization");
    let (task, completion) = finalizing.into_parts();
    task.handle.abort();
    let mut legacy_task = running_task(Arc::clone(&turn_context));
    legacy_task._agent_execution_guard = control.execution_guard(MultiAgentVersion::V2, &source);
    slot.install_running_task_for_legacy_start(legacy_task);
    assert!(Arc::ptr_eq(
        completion.turn_state(),
        generation.turn_state()
    ));
    assert_at_capacity(&control, &source);
    assert!(slot.complete_finalization(completion).is_ok());
    drop(admit_guard(&control, &source));

    let guard = admit_guard(&control, &source);
    let Ok(driver) = slot.begin_fresh_start(Some(guard)) else {
        panic!("idle slot should accept exact execution capacity");
    };
    let generation = driver.generation();
    assert!(slot.cancel_start_exact(&generation, TurnAbortReason::Interrupted));
    assert!(slot.poison_abandoned_start(driver).is_ok());
    assert!(slot.reserve_taskless().is_none());
    assert_at_capacity(&control, &source);
}

#[tokio::test]
async fn abort_transition_tracks_starting_running_and_finalizing() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = SessionTurnSlot::default();
    let reserved_state = Arc::clone(slot.reserve_taskless().expect("idle slot should reserve"));
    let SessionTurnAbortTransition::Starting(reserved) =
        slot.begin_abort(TurnAbortReason::Replaced)
    else {
        panic!("reserved start should cancel and release its stored driver");
    };
    assert!(Arc::ptr_eq(reserved.turn_state(), &reserved_state));
    assert_eq!(
        reserved.finished_outcome(),
        Some(TurnStartOutcome::Cancelled(TurnAbortReason::Replaced))
    );
    assert!(slot.is_idle());

    let Ok(driver) = slot.begin_fresh_start(/*execution_guard*/ None) else {
        panic!("idle slot should start");
    };
    assert!(slot.reserve_taskless().is_none());
    let generation = driver.generation();
    let SessionTurnAbortTransition::Starting(starting) =
        slot.begin_abort(TurnAbortReason::Interrupted)
    else {
        panic!("starting turn should be cancelled in place");
    };
    assert!(Arc::ptr_eq(starting.turn_state(), generation.turn_state()));
    let lifecycle_finished = generation.wait_lifecycle_finished();
    tokio::pin!(lifecycle_finished);
    assert!(matches!(
        futures::poll!(&mut lifecycle_finished),
        Poll::Pending
    ));
    let Ok(reason) = slot.complete_cancelled_start(driver) else {
        panic!("exact driver should compensate the cancelled start");
    };
    assert_eq!(reason, TurnAbortReason::Interrupted);
    timeout(Duration::from_secs(/*secs*/ 1), &mut lifecycle_finished)
        .await
        .expect("compensated start should finish its lifecycle");
    assert!(matches!(
        slot.begin_abort(TurnAbortReason::Interrupted),
        SessionTurnAbortTransition::Inactive
    ));
    let Ok(driver) = slot.begin_fresh_start(/*execution_guard*/ None) else {
        panic!("idle successor should start");
    };
    let generation = driver.generation();
    assert!(
        slot.commit_start(driver, running_task(Arc::clone(&turn_context)))
            .is_ok()
    );
    let SessionTurnAbortTransition::Running(finalizing) =
        slot.begin_abort(TurnAbortReason::Interrupted)
    else {
        panic!("running turn should transfer into finalization");
    };
    let (task, completion) = finalizing.into_parts();
    task.handle.abort();
    assert!(slot.reserve_taskless().is_none());
    let SessionTurnAbortTransition::Finalizing(waiting) =
        slot.begin_abort(TurnAbortReason::Interrupted)
    else {
        panic!("an existing finalization should be observed, not replaced");
    };
    assert!(Arc::ptr_eq(waiting.turn_state(), generation.turn_state()));
    assert!(slot.complete_finalization(completion).is_ok());
}

#[tokio::test]
async fn stale_finalizer_cannot_complete_or_poison_reopened_same_owner() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = SessionTurnSlot::default();
    let Ok(driver) = slot.begin_fresh_start(/*execution_guard*/ None) else {
        panic!("idle slot should start");
    };
    let generation = driver.generation();
    assert!(
        slot.commit_start(driver, running_task(Arc::clone(&turn_context)))
            .is_ok()
    );
    let first = slot
        .begin_finalization(&generation, &turn_context)
        .expect("first finalization");
    let (first_task, first_completion) = first.into_parts();
    first_task.handle.abort();
    assert!(matches!(
        slot.lifecycle
            .install_running_task_for_legacy(running_task(Arc::clone(&turn_context))),
        Ok(None)
    ));
    let second = slot
        .begin_finalization(&generation, &turn_context)
        .expect("reopened owner should receive a new finalization token");
    let (second_task, second_completion) = second.into_parts();
    second_task.handle.abort();
    let Err(first_completion) = slot.complete_finalization(first_completion) else {
        panic!("stale completion must not finish a reopened owner");
    };
    assert!(
        slot.poison_abandoned_finalization(first_completion)
            .is_err()
    );
    let lifecycle_finished = generation.wait_lifecycle_finished();
    tokio::pin!(lifecycle_finished);
    assert!(matches!(
        futures::poll!(&mut lifecycle_finished),
        Poll::Pending
    ));
    assert!(slot.has_active_turn());
    assert!(slot.complete_finalization(second_completion).is_ok());
    timeout(Duration::from_secs(/*secs*/ 1), &mut lifecycle_finished)
        .await
        .expect("current finalization should finish the lifecycle");
}

use super::*;
use crate::session::tests::make_session_and_context;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

struct TestTask(Arc<TurnContext>, &'static str);

impl TurnLifecycleTask for TestTask {
    fn turn_context(&self) -> &Arc<TurnContext> {
        &self.0
    }
}

#[tokio::test]
async fn exact_start_controls_reject_foreign_and_stale_generations() {
    let mut slot = TurnLifecycleSlot::<String, TestTask>::default();
    let first_driver = slot.start("first lease".to_string()).expect("idle slot");
    let first = first_driver.generation();
    let mut other_slot = TurnLifecycleSlot::<String, TestTask>::default();
    let other_driver = other_slot
        .start("other lease".to_string())
        .expect("other idle slot");
    let other = other_driver.generation();

    assert_eq!(
        slot.replace_start_lease(&other_driver, "foreign lease".to_string()),
        Err("foreign lease".to_string())
    );
    assert_eq!(
        slot.replace_start_lease(&first_driver, "replacement lease".to_string()),
        Ok("first lease".to_string())
    );
    assert!(!slot.cancel_start_exact(&other, TurnAbortReason::Replaced));
    assert_eq!(first.cancel_reason(), None);
    assert!(slot.cancel_start_exact(&first, TurnAbortReason::Interrupted));
    assert_eq!(first.cancel_reason(), Some(TurnAbortReason::Interrupted));
    let Ok(reason) = slot.complete_cancelled_start(first_driver) else {
        panic!("exact driver should complete cancellation");
    };
    assert_eq!(reason, TurnAbortReason::Interrupted);

    let successor_driver = slot
        .start("successor lease".to_string())
        .expect("idle slot after cancellation");
    let successor = successor_driver.generation();
    assert!(!slot.cancel_start_exact(&first, TurnAbortReason::Replaced));
    assert_eq!(successor.cancel_reason(), None);

    assert!(slot.cancel_start_exact(&successor, TurnAbortReason::Interrupted));
    assert!(slot.complete_cancelled_start(successor_driver).is_ok());
    assert!(other_slot.cancel_start_exact(&other, TurnAbortReason::Interrupted));
    assert!(other_slot.complete_cancelled_start(other_driver).is_ok());
}

#[tokio::test]
async fn lifecycle_finishes_only_after_successful_finalization() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = TurnLifecycleSlot::<(), TestTask>::default();
    let driver = slot.start(()).expect("idle slot");
    let generation = driver.generation();

    assert!(
        slot.commit_start(driver, TestTask(Arc::clone(&turn_context), "task"))
            .is_ok()
    );
    assert_eq!(
        generation.wait_finished().await,
        TurnStartOutcome::Committed
    );
    assert!(!generation.lifecycle_finished.is_cancelled());
    let (_task, finalization) = slot
        .begin_finalization(&generation, &turn_context)
        .expect("exact owner should finalize");
    let finalizing_generation = slot
        .finalizing_generation()
        .expect("generation should remain projected while finalizing");
    assert!(finalizing_generation.matches(&generation));
    assert!(!generation.lifecycle_finished.is_cancelled());

    assert!(slot.complete_finalization(finalization).is_ok());
    generation.wait_lifecycle_finished().await;
}

#[tokio::test]
async fn cancelled_and_poisoned_transitions_finish_the_lifecycle() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = TurnLifecycleSlot::<(), TestTask>::default();
    let cancelled_driver = slot.start(()).expect("idle slot");
    let cancelled = cancelled_driver.generation();
    assert!(slot.cancel_start_exact(&cancelled, TurnAbortReason::Interrupted));
    assert!(!cancelled.lifecycle_finished.is_cancelled());
    assert!(slot.complete_cancelled_start(cancelled_driver).is_ok());
    assert!(cancelled.lifecycle_finished.is_cancelled());
    assert_eq!(
        cancelled.wait_finished().await,
        TurnStartOutcome::Cancelled(TurnAbortReason::Interrupted)
    );

    let abandoned_driver = slot.start(()).expect("idle slot");
    let abandoned = abandoned_driver.generation();
    assert!(slot.cancel_start_exact(&abandoned, TurnAbortReason::Replaced));
    assert!(slot.poison_abandoned_start(abandoned_driver).is_ok());
    abandoned.wait_lifecycle_finished().await;
    assert_eq!(
        abandoned.wait_finished().await,
        TurnStartOutcome::Poisoned(TurnAbortReason::Replaced)
    );

    let mut finalizing_slot = TurnLifecycleSlot::<(), TestTask>::default();
    let finalizing_driver = finalizing_slot.start(()).expect("idle slot");
    let finalizing = finalizing_driver.generation();
    assert!(
        finalizing_slot
            .commit_start(
                finalizing_driver,
                TestTask(Arc::clone(&turn_context), "finalizing"),
            )
            .is_ok()
    );
    let (_task, finalization) = finalizing_slot
        .begin_finalization(&finalizing, &turn_context)
        .expect("exact owner should finalize");
    assert!(finalizing_slot.poison_finalization(finalization).is_ok());
    finalizing.wait_lifecycle_finished().await;
}

#[tokio::test]
async fn abandoned_start_driver_publishes_poison_and_leaves_slot_fail_closed() {
    let mut slot = TurnLifecycleSlot::<String, TestTask>::default();
    let driver = slot.start("held lease".to_string()).expect("idle slot");
    let generation = driver.generation();

    drop(driver);

    assert_eq!(
        generation.wait_finished().await,
        TurnStartOutcome::Poisoned(TurnAbortReason::Interrupted)
    );
    generation.wait_lifecycle_finished().await;
    let Err(lease) = slot.start("successor lease".to_string()) else {
        panic!("abandoned start must leave the slot fail closed");
    };
    assert_eq!(lease, "successor lease");
}

#[tokio::test]
async fn abandoned_finalization_authority_finishes_lifecycle_and_leaves_slot_fail_closed() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = TurnLifecycleSlot::<String, TestTask>::default();
    let driver = slot.start("held lease".to_string()).expect("idle slot");
    let generation = driver.generation();
    assert!(
        slot.commit_start(driver, TestTask(Arc::clone(&turn_context), "task"))
            .is_ok()
    );
    let (_task, finalization) = slot
        .begin_finalization(&generation, &turn_context)
        .expect("exact owner should finalize");

    drop(finalization);

    generation.wait_lifecycle_finished().await;
    assert!(
        slot.finalizing_generation()
            .is_some_and(|current| current.matches(&generation))
    );
    let Err(replacement) = slot
        .install_running_task_for_legacy(TestTask(Arc::clone(&turn_context), "replacement task"))
    else {
        panic!("abandoned finalization must not be reopened by a legacy install");
    };
    assert_eq!(replacement.1, "replacement task");
    assert!(
        slot.finalizing_generation()
            .is_some_and(|current| current.matches(&generation))
    );
    let Err(lease) = slot.start("successor lease".to_string()) else {
        panic!("abandoned finalization must leave the slot fail closed");
    };
    assert_eq!(lease, "successor lease");
}

#[tokio::test]
async fn cancelled_start_is_non_committable_until_compensated() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);
    let mut slot = TurnLifecycleSlot::<String, TestTask>::default();
    let driver = slot.start("capacity".to_string()).expect("idle slot");
    let mut other_slot = TurnLifecycleSlot::<String, TestTask>::default();
    let foreign_driver = other_slot
        .start("foreign capacity".to_string())
        .expect("other idle slot");
    let foreign_generation = foreign_driver.generation();
    let Err((foreign_driver, task)) = slot.commit_start(
        foreign_driver,
        TestTask(Arc::clone(&turn_context), "foreign"),
    ) else {
        panic!("foreign driver must not commit");
    };
    assert_eq!(task.1, "foreign");
    let control = slot
        .cancel_start(TurnAbortReason::Replaced)
        .expect("current start should cancel");
    assert_eq!(control.cancelled().await, TurnAbortReason::Replaced);
    assert!(slot.cancel_start(TurnAbortReason::Interrupted).is_some());
    let Err(foreign_driver) = slot.poison_abandoned_start(foreign_driver) else {
        panic!("foreign driver must not poison the active start");
    };
    assert_eq!(foreign_generation.finished_outcome(), None);
    let Err(foreign_driver) = slot.complete_cancelled_start(foreign_driver) else {
        panic!("foreign driver must not compensate");
    };
    drop(foreign_driver);
    let Err((driver, task)) = slot.commit_start(driver, TestTask(turn_context, "task")) else {
        panic!("cancelled start must not commit");
    };
    assert_eq!(task.1, "task");
    let Ok(reason) = slot.complete_cancelled_start(driver) else {
        panic!("linear driver should complete compensation");
    };
    assert_eq!(reason, TurnAbortReason::Replaced);
    assert_eq!(
        control.wait_finished().await,
        TurnStartOutcome::Cancelled(TurnAbortReason::Replaced)
    );
    let abandoned_driver = slot.start("held".to_string()).expect("idle slot");
    let abandoned = abandoned_driver.generation();
    assert!(slot.cancel_start(TurnAbortReason::Interrupted).is_some());
    assert!(slot.poison_abandoned_start(abandoned_driver).is_ok());
    assert_eq!(
        abandoned.wait_finished().await,
        TurnStartOutcome::Poisoned(TurnAbortReason::Interrupted)
    );
}
#[tokio::test]
async fn exact_context_owns_running_and_finalizing_generation() {
    let (session, first_context) = make_session_and_context().await;
    let second_context = session
        .new_default_turn_with_sub_id(first_context.sub_id.clone())
        .await;
    let first_context = Arc::new(first_context);
    let second_context = Arc::new(second_context);
    let mut slot = TurnLifecycleSlot::<String, TestTask>::default();
    let first_driver = slot.start("first lease".to_string()).expect("idle slot");
    let first = first_driver.generation();
    assert!(slot.start("busy starting".to_string()).is_err());
    assert!(
        slot.commit_start(
            first_driver,
            TestTask(Arc::clone(&first_context), "first task")
        )
        .is_ok()
    );
    assert!(slot.start("busy running".to_string()).is_err());
    assert_eq!(first.wait_finished().await, TurnStartOutcome::Committed);
    assert!(slot.begin_finalization(&first, &second_context).is_none());
    let (task, finalization) = slot
        .begin_finalization(&first, &first_context)
        .expect("exact owner should finalize");
    assert!(slot.start("busy finalizing".to_string()).is_err());
    assert_eq!(task.1, "first task");
    let mut other_slot = TurnLifecycleSlot::<(), TestTask>::default();
    let other_driver = other_slot.start(()).expect("other idle slot");
    let other = other_driver.generation();
    assert!(
        other_slot
            .commit_start(
                other_driver,
                TestTask(Arc::clone(&first_context), "other task")
            )
            .is_ok()
    );
    let (_task, other_finalization) = other_slot
        .begin_finalization(&other, &first_context)
        .expect("other exact owner should finalize");
    assert!(slot.complete_finalization(other_finalization).is_err());
    assert!(other_slot.poison_finalization_for_turn(&other, &first_context));
    let Ok(()) = slot.complete_finalization(finalization) else {
        panic!("exact finalizer should complete");
    };
    let second_driver = slot.start("second lease".to_string()).expect("idle slot");
    let second = second_driver.generation();
    assert!(
        slot.commit_start(
            second_driver,
            TestTask(Arc::clone(&second_context), "second task")
        )
        .is_ok()
    );
    assert!(slot.begin_finalization(&first, &second_context).is_none());
    let (_task, finalization) = slot
        .begin_finalization(&second, &second_context)
        .expect("exact successor should finalize");
    assert!(!slot.poison_finalization_for_turn(&first, &second_context));
    assert!(slot.poison_finalization(finalization).is_ok());
    let Err(lease) = slot.start("successor capacity".to_string()) else {
        panic!("poisoned slot must reject a successor");
    };
    assert_eq!(lease, "successor capacity");
}

#[tokio::test]
async fn legacy_running_replacement_preserves_generation_and_state() {
    let (session, first_context) = make_session_and_context().await;
    let first_context = Arc::new(first_context);
    let second_context = session.new_default_turn().await;
    let mut slot = TurnLifecycleSlot::<(), TestTask>::default();
    let driver = slot.start(()).expect("idle slot");
    let generation = driver.generation();
    let turn_state = Arc::clone(generation.turn_state());
    assert!(
        slot.commit_start(driver, TestTask(first_context, "first"))
            .is_ok()
    );

    let Ok(Some(replaced)) =
        slot.install_running_task_for_legacy(TestTask(Arc::clone(&second_context), "second"))
    else {
        panic!("running replacement should return the old task");
    };
    assert_eq!(replaced.1, "first");
    let (running_generation, running_context, task) = slot.running().expect("replacement running");
    assert!(running_generation.matches(&generation));
    assert!(Arc::ptr_eq(running_generation.turn_state(), &turn_state));
    assert!(Arc::ptr_eq(running_context, &second_context));
    assert_eq!(task.1, "second");
    let (task, finalization) = slot
        .begin_finalization(&generation, &second_context)
        .expect("replacement should finalize under the original generation");
    assert_eq!(task.1, "second");
    assert!(slot.complete_finalization(finalization).is_ok());
}

#[derive(Debug)]
struct DropLease(Arc<AtomicUsize>);

impl Drop for DropLease {
    fn drop(&mut self) {
        self.0.fetch_add(/*val*/ 1, Ordering::SeqCst);
    }
}

fn tracked_lease() -> (DropLease, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(/*v*/ 0));
    (DropLease(Arc::clone(&drops)), drops)
}

#[tokio::test]
async fn lease_is_released_only_after_success_and_retained_while_poisoned() {
    let (_session, turn_context) = make_session_and_context().await;
    let turn_context = Arc::new(turn_context);

    let (lease, drops) = tracked_lease();
    let mut slot = TurnLifecycleSlot::<DropLease, TestTask>::default();
    let driver = slot.start(lease).expect("idle slot");
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(slot.cancel_start(TurnAbortReason::Interrupted).is_some());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(slot.complete_cancelled_start(driver).is_ok());
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    let (lease, drops) = tracked_lease();
    let driver = slot.start(lease).expect("idle slot");
    let generation = driver.generation();
    assert!(
        slot.commit_start(driver, TestTask(Arc::clone(&turn_context), "task"))
            .is_ok()
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let (_task, finalization) = slot
        .begin_finalization(&generation, &turn_context)
        .expect("exact owner should finalize");
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(slot.complete_finalization(finalization).is_ok());
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    for poison_during_finalization in [false, true] {
        let (lease, drops) = tracked_lease();
        let mut poisoned_slot = TurnLifecycleSlot::<DropLease, TestTask>::default();
        let driver = poisoned_slot.start(lease).expect("idle slot");
        let generation = driver.generation();
        if poison_during_finalization {
            assert!(
                poisoned_slot
                    .commit_start(driver, TestTask(Arc::clone(&turn_context), "task"))
                    .is_ok()
            );
            let (_task, finalization) = poisoned_slot
                .begin_finalization(&generation, &turn_context)
                .expect("exact owner should finalize");
            assert!(poisoned_slot.poison_finalization(finalization).is_ok());
        } else {
            assert!(
                poisoned_slot
                    .cancel_start(TurnAbortReason::Interrupted)
                    .is_some()
            );
            assert!(poisoned_slot.poison_abandoned_start(driver).is_ok());
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(poisoned_slot);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

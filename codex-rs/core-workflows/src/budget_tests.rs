use std::sync::Arc;
use std::sync::Barrier;

use pretty_assertions::assert_eq;

use super::WorkflowBudget;
use super::WorkflowBudgetLimit;

#[test]
fn unmetered_budget_admits_and_reports_no_remaining_ceiling() {
    let budget = WorkflowBudget::new(WorkflowBudgetLimit::Unmetered);
    let reservation = budget.reserve(100).expect("unmetered admission");
    budget.record_spent(40);

    assert_eq!(
        budget.snapshot(),
        super::WorkflowBudgetSnapshot {
            limit: WorkflowBudgetLimit::Unmetered,
            spent: 40,
            remaining: None,
        }
    );
    assert!(!budget.is_exhausted());
    drop(reservation);
}

#[test]
fn explicit_zero_rejects_the_first_agent() {
    let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(0));

    assert!(budget.reserve(1).is_err());
    assert!(budget.is_exhausted());
}

#[test]
fn concurrent_reservations_serialize_at_the_ceiling() {
    const ATTEMPTS: usize = 16;
    let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(10));
    let barrier = Arc::new(Barrier::new(ATTEMPTS));
    let threads = (0..ATTEMPTS)
        .map(|_| {
            let budget = Arc::clone(&budget);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                budget.reserve(10).ok()
            })
        })
        .collect::<Vec<_>>();
    let reservations = threads
        .into_iter()
        .filter_map(|thread| thread.join().expect("reservation thread"))
        .collect::<Vec<_>>();

    assert_eq!(reservations.len(), 1);
}

#[test]
fn dropping_a_reservation_restores_admission_headroom() {
    let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(10));
    let reservation = budget.reserve(10).expect("first admission");
    assert!(budget.reserve(10).is_err());

    drop(reservation);

    assert!(budget.reserve(10).is_ok());
}

#[test]
fn nested_spend_and_reservations_roll_up_to_parent() {
    let parent = WorkflowBudget::new(WorkflowBudgetLimit::Limited(100));
    let child = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Limited(1_000));
    let reservation = child.reserve(100).expect("nested admission");
    assert!(child.reserve(1).is_err(), "parent ceiling must clamp child");
    child.record_spent(35);
    drop(reservation);

    assert_eq!(child.snapshot().spent, 35);
    assert_eq!(parent.snapshot().spent, 35);
    assert_eq!(parent.snapshot().remaining, Some(65));
}

#[test]
fn descendant_rejection_rolls_back_ancestor_reservation() {
    let parent = WorkflowBudget::new(WorkflowBudgetLimit::Limited(100));
    let child = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Limited(0));

    assert!(child.reserve(100).is_err());
    assert!(parent.reserve(100).is_ok());
}

#[test]
fn sibling_runs_share_the_parent_ceiling_but_not_local_spend() {
    let parent = WorkflowBudget::new(WorkflowBudgetLimit::Limited(20));
    let left = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Unmetered);
    let right = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Unmetered);
    let left_reservation = left.reserve(20).expect("left admitted");

    assert!(right.reserve(1).is_err());
    left.record_spent(7);
    drop(left_reservation);

    assert_eq!(left.snapshot().spent, 7);
    assert_eq!(right.snapshot().spent, 0);
    assert_eq!(parent.snapshot().spent, 7);
}

#[test]
fn replayed_spend_exhausts_at_the_original_boundary() {
    let budget = WorkflowBudget::new(WorkflowBudgetLimit::Limited(50));
    budget.record_replayed_spent(20);
    budget.record_replayed_spent(30);

    assert_eq!(budget.snapshot().spent, 50);
    assert!(budget.is_exhausted());
    assert!(budget.reserve(1).is_err());
}

#[test]
fn unmetered_child_inherits_parent_view() {
    let parent = WorkflowBudget::new(WorkflowBudgetLimit::Limited(100));
    parent.record_spent(25);
    let child = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Unmetered);

    assert_eq!(child.effective_snapshot(), parent.effective_snapshot());
}

#[test]
fn explicit_child_ceiling_only_tightens_parent_headroom() {
    let parent = WorkflowBudget::new(WorkflowBudgetLimit::Limited(100));
    parent.record_spent(25);
    let child = WorkflowBudget::child(Arc::clone(&parent), WorkflowBudgetLimit::Limited(20));

    assert_eq!(
        child.effective_snapshot(),
        super::WorkflowBudgetSnapshot {
            limit: WorkflowBudgetLimit::Limited(20),
            spent: 0,
            remaining: Some(20),
        }
    );

    child.record_spent(8);
    assert_eq!(child.effective_snapshot().spent, 8);
    assert_eq!(child.effective_snapshot().remaining, Some(12));
    assert_eq!(parent.effective_snapshot().spent, 33);
}

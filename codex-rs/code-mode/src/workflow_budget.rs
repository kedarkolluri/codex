use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering;

use codex_code_mode_protocol::WorkflowBudgetHandle;
use codex_code_mode_protocol::WorkflowBudgetSnapshot;

/// Runtime-owned mirror of core's run-local workflow budget.
///
/// The initial value travels with `ExecuteRequest`; host callbacks refresh the
/// mirror before their JS promises settle. Atomic monotonic updates make the
/// native getters safe on the isolate thread without coupling it to either the
/// in-process or stdio host implementation.
pub(crate) struct WorkflowBudgetMirror {
    total: i64,
    spent: AtomicI64,
    remaining: AtomicI64,
}

impl WorkflowBudgetMirror {
    pub(crate) fn new(snapshot: WorkflowBudgetSnapshot) -> Self {
        Self {
            total: saturating_i64(snapshot.total.unwrap_or(0)),
            spent: AtomicI64::new(saturating_i64(snapshot.spent)),
            remaining: AtomicI64::new(saturating_i64(snapshot.remaining.unwrap_or(0))),
        }
    }

    pub(crate) fn update(&self, snapshot: WorkflowBudgetSnapshot) {
        self.spent
            .fetch_max(saturating_i64(snapshot.spent), Ordering::AcqRel);
        self.remaining.fetch_min(
            saturating_i64(snapshot.remaining.unwrap_or(0)),
            Ordering::AcqRel,
        );
    }
}

impl WorkflowBudgetHandle for WorkflowBudgetMirror {
    fn total(&self) -> i64 {
        self.total
    }

    fn spent(&self) -> i64 {
        self.spent.load(Ordering::Acquire)
    }

    fn remaining(&self) -> i64 {
        self.remaining.load(Ordering::Acquire)
    }
}

fn saturating_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
#[path = "workflow_budget_tests.rs"]
mod tests;

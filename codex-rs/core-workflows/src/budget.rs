use std::sync::Arc;
use std::sync::Mutex;

/// Optional hard output-token ceiling for one workflow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowBudgetLimit {
    /// No run-specific ceiling. An ancestor may still impose one.
    Unmetered,
    /// A real ceiling, including zero.
    Limited(u64),
}

/// Copyable view used by workflow globals and remote-host mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowBudgetSnapshot {
    pub limit: WorkflowBudgetLimit,
    pub spent: u64,
    pub remaining: Option<u64>,
}

/// Admission failure from this run or one of its ancestor workflow runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowBudgetExceeded;

impl std::fmt::Display for WorkflowBudgetExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("workflow budget exceeded")
    }
}

impl std::error::Error for WorkflowBudgetExceeded {}

/// Run-local workflow budget whose spend rolls up through nested-run ancestors.
///
/// This meter is deliberately independent from Codex's session-wide rollout
/// budget. A workflow child checks both governors, while mutating neither one's
/// configuration. Nested runs hold a parent meter so their live and replayed
/// spend remains charged to every enclosing workflow ceiling.
pub struct WorkflowBudget {
    limit: WorkflowBudgetLimit,
    state: Mutex<WorkflowBudgetState>,
    parent: Option<Arc<WorkflowBudget>>,
}

#[derive(Default)]
struct WorkflowBudgetState {
    spent: u64,
    reserved: u64,
}

impl WorkflowBudget {
    pub fn new(limit: WorkflowBudgetLimit) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new(WorkflowBudgetState::default()),
            parent: None,
        })
    }

    pub fn child(parent: Arc<Self>, limit: WorkflowBudgetLimit) -> Arc<Self> {
        Arc::new(Self {
            limit,
            state: Mutex::new(WorkflowBudgetState::default()),
            parent: Some(parent),
        })
    }

    /// Reserve one in-flight agent estimate against this run and every ancestor.
    ///
    /// Ancestors are always acquired first, giving all nested runs one lock order.
    /// If a tighter descendant rejects, already-acquired ancestor reservations are
    /// rolled back by the returned guards' `Drop` implementation.
    pub fn reserve(
        self: &Arc<Self>,
        estimate: u64,
    ) -> Result<WorkflowBudgetReservation, WorkflowBudgetExceeded> {
        let parent = self
            .parent
            .as_ref()
            .map(|parent| parent.reserve(estimate))
            .transpose()?;
        let reserved = match self.limit {
            WorkflowBudgetLimit::Unmetered => false,
            WorkflowBudgetLimit::Limited(limit) => {
                let mut state = self.lock();
                if state.spent.saturating_add(state.reserved) >= limit {
                    return Err(WorkflowBudgetExceeded);
                }
                state.reserved = state.reserved.saturating_add(estimate);
                true
            }
        };
        Ok(WorkflowBudgetReservation {
            budget: Arc::clone(self),
            estimate,
            reserved,
            parent: parent.map(Box::new),
        })
    }

    /// Add output tokens from a live child to this run and all ancestors.
    pub fn record_spent(&self, tokens: u64) {
        if let Some(parent) = self.parent.as_ref() {
            parent.record_spent(tokens);
        }
        let mut state = self.lock();
        state.spent = state.spent.saturating_add(tokens);
    }

    /// Re-add journaled output tokens while replaying an unchanged prefix.
    pub fn record_replayed_spent(&self, tokens: u64) {
        self.record_spent(tokens);
    }

    pub fn snapshot(&self) -> WorkflowBudgetSnapshot {
        let state = self.lock();
        let remaining = match self.limit {
            WorkflowBudgetLimit::Unmetered => None,
            WorkflowBudgetLimit::Limited(limit) => Some(limit.saturating_sub(state.spent)),
        };
        WorkflowBudgetSnapshot {
            limit: self.limit,
            spent: state.spent,
            remaining,
        }
    }

    /// Return the budget view exposed by this run's workflow globals.
    ///
    /// An unmetered child inherits its parent's complete view. A child with an
    /// explicit ceiling reports its own spend, while its remaining headroom is
    /// clamped by every ancestor. This keeps sibling-local spend isolated while
    /// still allowing an enclosing workflow to govern its entire nested tree.
    pub fn effective_snapshot(&self) -> WorkflowBudgetSnapshot {
        let local = self.snapshot();
        let Some(parent) = self.parent.as_ref() else {
            return local;
        };
        let parent = parent.effective_snapshot();
        match local.limit {
            WorkflowBudgetLimit::Unmetered => parent,
            WorkflowBudgetLimit::Limited(limit) => {
                let local_remaining = limit.saturating_sub(local.spent);
                let remaining = Some(match parent.remaining {
                    Some(parent_remaining) => local_remaining.min(parent_remaining),
                    None => local_remaining,
                });
                WorkflowBudgetSnapshot {
                    limit: WorkflowBudgetLimit::Limited(
                        local.spent.saturating_add(remaining.unwrap_or(0)),
                    ),
                    spent: local.spent,
                    remaining,
                }
            }
        }
    }

    pub fn is_exhausted(&self) -> bool {
        let snapshot = self.snapshot();
        snapshot.remaining == Some(0)
            || self
                .parent
                .as_ref()
                .is_some_and(|parent| parent.is_exhausted())
    }

    fn release(&self, estimate: u64) {
        let mut state = self.lock();
        state.reserved = state.reserved.saturating_sub(estimate);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, WorkflowBudgetState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// Cancellation-safe reservation held for one admitted workflow agent.
pub struct WorkflowBudgetReservation {
    budget: Arc<WorkflowBudget>,
    estimate: u64,
    reserved: bool,
    parent: Option<Box<WorkflowBudgetReservation>>,
}

impl Drop for WorkflowBudgetReservation {
    fn drop(&mut self) {
        if self.reserved {
            self.budget.release(self.estimate);
        }
        drop(self.parent.take());
    }
}

#[cfg(test)]
#[path = "budget_tests.rs"]
mod tests;

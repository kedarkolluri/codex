use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

#[derive(Default)]
struct TriggerTurnRetryState {
    waiting_for_capacity: AtomicBool,
}

/// Coalesces automatic trigger-turn retries while execution capacity is full.
#[allow(dead_code)] // Activated by the atomic task-start stage.
#[derive(Default)]
pub(super) struct TriggerTurnRetry {
    state: Arc<TriggerTurnRetryState>,
}

/// Exact ownership of the one capacity wait currently driving a retry.
#[must_use = "the claim must be held until the capacity wait finishes"]
pub(super) struct TriggerTurnRetryClaim {
    state: Arc<TriggerTurnRetryState>,
}

#[allow(dead_code)] // Activated by the atomic task-start stage.
impl TriggerTurnRetry {
    pub(super) fn try_begin_wait(&self) -> Option<TriggerTurnRetryClaim> {
        self.state
            .waiting_for_capacity
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| TriggerTurnRetryClaim {
                state: Arc::clone(&self.state),
            })
    }
}

#[allow(dead_code)] // Activated by the atomic task-start stage.
impl TriggerTurnRetryClaim {
    /// Releases the coalescing slot before retrying atomic admission.
    pub(super) fn release_for_retry(self) {
        drop(self);
    }
}

impl Drop for TriggerTurnRetryClaim {
    fn drop(&mut self) {
        self.state
            .waiting_for_capacity
            .store(false, Ordering::Release);
    }
}

#[cfg(test)]
#[path = "trigger_turn_retry_tests.rs"]
mod tests;

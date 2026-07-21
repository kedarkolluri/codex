use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use crate::agent::control::AgentExecutionCapacityWaiter;

use super::session::Session;

/// Coalesces automatic trigger-turn retries while execution capacity is full.
#[derive(Default)]
pub(super) struct TriggerTurnRetry {
    waiting_for_capacity: AtomicBool,
}

impl TriggerTurnRetry {
    fn try_begin_wait(&self) -> bool {
        self.waiting_for_capacity
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn finish_wait(&self) {
        self.waiting_for_capacity.store(false, Ordering::Release);
    }

    #[cfg(test)]
    fn is_waiting(&self) -> bool {
        self.waiting_for_capacity.load(Ordering::Acquire)
    }
}

impl Session {
    /// Waits for one execution lease release, then retries pending trigger-turn
    /// mailbox work. Concurrent capacity failures share the same waiting driver.
    pub(crate) fn schedule_trigger_turn_retry(
        self: &Arc<Self>,
        waiter: AgentExecutionCapacityWaiter,
    ) {
        if !self.trigger_turn_retry.try_begin_wait() {
            return;
        }

        let session = Arc::downgrade(self);
        self.services.runtime_handle.spawn(async move {
            waiter.wait_for_release().await;
            let Some(session) = session.upgrade() else {
                return;
            };
            // Release the coalescing slot before retrying. If another agent
            // claims the newly available lease first, the retry can arm the
            // next pre-subscribed waiter without losing that future release.
            session.trigger_turn_retry.finish_wait();
            session.maybe_start_turn_for_pending_work().await;
        });
    }
}

#[cfg(test)]
#[path = "trigger_turn_retry_tests.rs"]
mod tests;

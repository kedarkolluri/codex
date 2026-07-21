use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use crate::agent::control::AgentExecutionCapacityWaiter;

use super::session::Session;
use super::turn_start_gate::AutomaticTurnStartTicket;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AutomaticTicketInvalidation {
    Retry,
    Suppress,
}

pub(crate) struct PendingWorkStartRequest {
    sub_id: String,
    ticket: AutomaticTurnStartTicket,
    invalidation: AutomaticTicketInvalidation,
}

impl PendingWorkStartRequest {
    pub(crate) fn new(
        sub_id: String,
        ticket: AutomaticTurnStartTicket,
        invalidation: AutomaticTicketInvalidation,
    ) -> Self {
        Self {
            sub_id,
            ticket,
            invalidation,
        }
    }

    pub(crate) fn ticket(&self) -> AutomaticTurnStartTicket {
        self.ticket
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        String,
        AutomaticTurnStartTicket,
        AutomaticTicketInvalidation,
    ) {
        (self.sub_id, self.ticket, self.invalidation)
    }

    fn merge(&mut self, mut newer: Self) {
        if self.ticket == newer.ticket && self.invalidation == AutomaticTicketInvalidation::Retry {
            newer.invalidation = AutomaticTicketInvalidation::Retry;
        }
        *self = newer;
    }
}

#[derive(Default)]
enum CapacityRetryState {
    #[default]
    Idle,
    Waiting(PendingWorkStartRequest),
}

#[derive(Default)]
struct TriggerTurnRetryState {
    capacity: StdMutex<CapacityRetryState>,
}

/// Coalesces capacity waiters while retaining their authenticated start request.
#[derive(Clone, Default)]
pub(super) struct TriggerTurnRetry {
    state: Arc<TriggerTurnRetryState>,
}

#[must_use = "capacity retry ownership must be consumed or released"]
struct TriggerTurnCapacityClaim {
    state: Arc<TriggerTurnRetryState>,
    armed: bool,
}

impl TriggerTurnRetry {
    fn register_capacity_wait(
        &self,
        request: PendingWorkStartRequest,
    ) -> Option<TriggerTurnCapacityClaim> {
        let mut capacity = self
            .state
            .capacity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &mut *capacity {
            CapacityRetryState::Idle => {
                *capacity = CapacityRetryState::Waiting(request);
                Some(TriggerTurnCapacityClaim {
                    state: Arc::clone(&self.state),
                    armed: true,
                })
            }
            CapacityRetryState::Waiting(existing) => {
                existing.merge(request);
                None
            }
        }
    }
}

impl TriggerTurnCapacityClaim {
    fn take_request(mut self) -> PendingWorkStartRequest {
        let mut capacity = self
            .state
            .capacity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let CapacityRetryState::Waiting(request) =
            std::mem::replace(&mut *capacity, CapacityRetryState::Idle)
        else {
            unreachable!("capacity retry claim must own the waiting state");
        };
        self.armed = false;
        request
    }
}

impl Drop for TriggerTurnCapacityClaim {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut capacity = self
            .state
            .capacity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *capacity = CapacityRetryState::Idle;
    }
}

impl Session {
    /// Waits for one execution lease release, then retries the newest
    /// authenticated trigger-turn request. Concurrent capacity failures share
    /// one waiter without replacing its ticket with fresh authority.
    pub(crate) async fn schedule_trigger_turn_retry(
        self: &Arc<Self>,
        waiter: AgentExecutionCapacityWaiter,
        request: PendingWorkStartRequest,
    ) {
        let claim = {
            let _active_turn = self.active_turn.lock().await;
            if !self
                .turn_start_gate
                .admits_automatic_start(request.ticket())
            {
                return;
            }
            self.trigger_turn_retry.register_capacity_wait(request)
        };
        let Some(claim) = claim else {
            return;
        };

        let session = Arc::downgrade(self);
        self.services.runtime_handle.spawn(async move {
            waiter.wait_for_release().await;
            let request = claim.take_request();
            let Some(session) = session.upgrade() else {
                return;
            };
            session.drive_pending_work_start(request).await;
        });
    }
}

#[cfg(test)]
#[path = "trigger_turn_retry_tests.rs"]
mod tests;

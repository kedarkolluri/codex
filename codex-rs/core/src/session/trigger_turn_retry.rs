use std::sync::Arc;
use std::sync::Mutex as StdMutex;

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

#[cfg(test)]
#[path = "trigger_turn_retry_tests.rs"]
mod tests;

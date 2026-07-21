use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::Mutex;
use tokio::sync::OwnedMutexGuard;

/// Epoch-authenticated admission for one automatic turn-start attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AutomaticTurnStartTicket(u64);

/// Linear permission to perform pre-start work and reserve the session task slot.
///
/// The permit is released immediately after exact slot reservation so aborts
/// can cancel lifecycle callbacks without waiting behind the admission lane.
#[must_use = "turn-start admission must reach a terminal decision"]
pub(crate) struct TurnStartAdmissionPermit {
    _guard: OwnedMutexGuard<()>,
}

/// Permanently closes task-start admission when session teardown begins.
#[derive(Default)]
pub(crate) struct TurnStartGate {
    closed: AtomicBool,
    automatic_state: AtomicU64,
    start_lane: Arc<Mutex<()>>,
}

#[derive(Clone, Copy)]
enum AutomaticStartInvalidation {
    Retry,
    Suppress,
}

impl TurnStartGate {
    pub(crate) async fn acquire_start_permit(&self) -> TurnStartAdmissionPermit {
        TurnStartAdmissionPermit {
            _guard: Arc::clone(&self.start_lane).lock_owned().await,
        }
    }

    pub(crate) fn is_open(&self) -> bool {
        !self.closed.load(Ordering::Acquire)
    }

    pub(crate) fn close(&self) {
        self.suppress_automatic_starts();
        self.closed.store(true, Ordering::Release);
    }

    pub(crate) fn automatic_start_ticket(&self) -> Option<AutomaticTurnStartTicket> {
        self.is_open().then(|| {
            AutomaticTurnStartTicket(self.automatic_state.load(Ordering::Acquire))
        })
    }

    pub(crate) fn admits_automatic_start(&self, ticket: AutomaticTurnStartTicket) -> bool {
        self.is_open() && self.automatic_state.load(Ordering::Acquire) == ticket.0
    }

    pub(crate) fn retry_automatic_starts_after_invalidation(
        &self,
    ) -> AutomaticTurnStartTicket {
        self.invalidate_automatic_starts(AutomaticStartInvalidation::Retry)
    }

    pub(crate) fn suppress_automatic_starts(&self) -> AutomaticTurnStartTicket {
        self.invalidate_automatic_starts(AutomaticStartInvalidation::Suppress)
    }

    pub(crate) fn retry_ticket_after_invalidation(
        &self,
        ticket: AutomaticTurnStartTicket,
    ) -> Option<AutomaticTurnStartTicket> {
        let state = self.automatic_state.load(Ordering::Acquire);
        let expected = Self::next_automatic_state(ticket.0, AutomaticStartInvalidation::Retry);
        (self.is_open() && state == expected)
            .then_some(AutomaticTurnStartTicket(state))
    }

    fn invalidate_automatic_starts(
        &self,
        invalidation: AutomaticStartInvalidation,
    ) -> AutomaticTurnStartTicket {
        let previous = self.automatic_state.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |state| Some(Self::next_automatic_state(state, invalidation)),
        );
        let previous = match previous {
            Ok(previous) => previous,
            Err(_) => unreachable!("automatic-start invalidation always returns a successor"),
        };
        AutomaticTurnStartTicket(Self::next_automatic_state(previous, invalidation))
    }

    fn next_automatic_state(state: u64, invalidation: AutomaticStartInvalidation) -> u64 {
        let next_epoch = state.wrapping_add(/*rhs*/ 2) & !1;
        match invalidation {
            AutomaticStartInvalidation::Retry => next_epoch | 1,
            AutomaticStartInvalidation::Suppress => next_epoch,
        }
    }
}

#[cfg(test)]
#[path = "turn_start_gate_tests.rs"]
mod tests;

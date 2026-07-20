use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::oneshot;

#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum TurnAdmissionOutcome {
    Started,
    Busy,
    Rejected(String),
    SessionLoopStopped,
}

pub(super) struct TurnAdmission {
    response_tx: oneshot::Sender<TurnAdmissionOutcome>,
}

impl TurnAdmission {
    pub(super) fn resolve(self, outcome: TurnAdmissionOutcome) {
        let _ = self.response_tx.send(outcome);
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TurnAdmissionRegistrationError {
    Duplicate { submission_id: String },
    SessionLoopStopped,
}

#[derive(Default)]
struct TurnAdmissionRegistryState {
    stopped: bool,
    pending: HashMap<String, TurnAdmission>,
}

#[derive(Default)]
pub(super) struct TurnAdmissionRegistry {
    state: Mutex<TurnAdmissionRegistryState>,
}

impl TurnAdmissionRegistry {
    // Used by the next stacked workflow spawn-and-await consumer.
    #[allow(dead_code)]
    pub(super) fn register(
        self: &Arc<Self>,
        submission_id: String,
    ) -> Result<
        (
            TurnAdmissionRegistration,
            oneshot::Receiver<TurnAdmissionOutcome>,
        ),
        TurnAdmissionRegistrationError,
    > {
        let (response_tx, response_rx) = oneshot::channel();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.stopped {
            return Err(TurnAdmissionRegistrationError::SessionLoopStopped);
        }
        match state.pending.entry(submission_id.clone()) {
            Entry::Occupied(_) => {
                return Err(TurnAdmissionRegistrationError::Duplicate { submission_id });
            }
            Entry::Vacant(entry) => {
                entry.insert(TurnAdmission { response_tx });
            }
        }
        drop(state);
        Ok((
            TurnAdmissionRegistration {
                registry: Arc::clone(self),
                submission_id,
                remove_on_drop: true,
            },
            response_rx,
        ))
    }

    #[allow(dead_code)]
    pub(super) fn take(&self, submission_id: &str) -> Option<TurnAdmission> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(submission_id)
    }

    #[allow(dead_code)]
    fn remove(&self, submission_id: &str) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .remove(submission_id);
    }

    fn fail_all(&self) {
        let pending = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.stopped = true;
            std::mem::take(&mut state.pending)
        };
        for admission in pending.into_values() {
            admission.resolve(TurnAdmissionOutcome::SessionLoopStopped);
        }
    }

    pub(super) fn loop_guard(self: &Arc<Self>) -> TurnAdmissionLoopGuard {
        TurnAdmissionLoopGuard {
            registry: Arc::clone(self),
        }
    }
}

/// Removes an admission when enqueueing its submission is cancelled or fails.
#[allow(dead_code)]
pub(super) struct TurnAdmissionRegistration {
    registry: Arc<TurnAdmissionRegistry>,
    submission_id: String,
    remove_on_drop: bool,
}

#[allow(dead_code)]
impl TurnAdmissionRegistration {
    /// Transfers cleanup ownership to the queued submission and session loop.
    pub(super) fn commit(&mut self) {
        self.remove_on_drop = false;
    }
}

impl Drop for TurnAdmissionRegistration {
    fn drop(&mut self) {
        if self.remove_on_drop {
            self.registry.remove(&self.submission_id);
        }
    }
}

/// Fails admissions left behind by channel close, shutdown, cancellation, or panic.
pub(super) struct TurnAdmissionLoopGuard {
    registry: Arc<TurnAdmissionRegistry>,
}

impl Drop for TurnAdmissionLoopGuard {
    fn drop(&mut self) {
        self.registry.fail_all();
    }
}

#[cfg(test)]
#[path = "turn_admission_registry_tests.rs"]
mod tests;

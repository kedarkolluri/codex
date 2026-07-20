use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::Mutex;

use tokio::sync::oneshot;

/// Result of asking the serialized session loop to start a user-input turn only while idle.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StartTurnIfIdleOutcome {
    Started { submission_id: String },
    Busy,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum TurnAdmissionOutcome {
    Started,
    Busy,
    Failed(String),
}

pub(super) struct TurnAdmission {
    response_tx: oneshot::Sender<TurnAdmissionOutcome>,
}

impl TurnAdmission {
    pub(super) fn resolve(self, outcome: TurnAdmissionOutcome) {
        let _ = self.response_tx.send(outcome);
    }
}

#[derive(Default)]
pub(super) struct TurnAdmissionRegistry {
    pending: Mutex<HashMap<String, TurnAdmission>>,
}

impl TurnAdmissionRegistry {
    pub(super) fn register(
        self: &Arc<Self>,
        submission_id: String,
    ) -> Result<
        (
            TurnAdmissionRegistration,
            oneshot::Receiver<TurnAdmissionOutcome>,
        ),
        String,
    > {
        let (response_tx, response_rx) = oneshot::channel();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match pending.entry(submission_id.clone()) {
            Entry::Occupied(_) => {
                return Err(format!(
                    "turn admission `{submission_id}` is already registered"
                ));
            }
            Entry::Vacant(entry) => {
                entry.insert(TurnAdmission { response_tx });
            }
        }
        drop(pending);
        Ok((
            TurnAdmissionRegistration {
                registry: Arc::clone(self),
                submission_id,
                remove_on_drop: true,
            },
            response_rx,
        ))
    }

    pub(super) fn take(&self, submission_id: &str) -> Option<TurnAdmission> {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(submission_id)
    }

    fn remove(&self, submission_id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(submission_id);
    }

    fn fail_all(&self) {
        let pending = std::mem::take(
            &mut *self
                .pending
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        for admission in pending.into_values() {
            admission.resolve(TurnAdmissionOutcome::Failed(
                "session loop stopped before turn admission".to_string(),
            ));
        }
    }

    pub(super) fn loop_guard(self: &Arc<Self>) -> TurnAdmissionLoopGuard {
        TurnAdmissionLoopGuard {
            registry: Arc::clone(self),
        }
    }
}

/// Removes a registered admission if enqueueing its submission is cancelled or fails.
pub(super) struct TurnAdmissionRegistration {
    registry: Arc<TurnAdmissionRegistry>,
    submission_id: String,
    remove_on_drop: bool,
}

impl TurnAdmissionRegistration {
    /// Transfers ownership of cleanup to the queued submission and the session loop.
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

/// Fails admissions left behind by channel close, shutdown, task cancellation, or panic.
pub(super) struct TurnAdmissionLoopGuard {
    registry: Arc<TurnAdmissionRegistry>,
}

impl Drop for TurnAdmissionLoopGuard {
    fn drop(&mut self) {
        self.registry.fail_all();
    }
}

#[cfg(test)]
#[path = "turn_admission_tests.rs"]
mod tests;

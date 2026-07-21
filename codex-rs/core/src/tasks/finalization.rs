use std::sync::Arc;

use crate::session::session::Session;
use crate::state::SessionTurnFinalization;
use crate::state::TurnState;

#[allow(dead_code)] // Activated by the atomic task-start stage.
#[derive(Debug, Eq, PartialEq)]
pub(super) enum PendingFinalizationOutcome {
    Completed,
    Displaced,
}

/// Linear authority for completing one exact finalizing turn.
#[allow(dead_code)] // Activated by the atomic task-start stage.
#[must_use = "an exact finalization must be completed or poisoned"]
pub(super) struct PendingFinalization {
    session: Arc<Session>,
    completion: Option<SessionTurnFinalization>,
}

#[allow(dead_code)] // Activated by the atomic task-start stage.
impl PendingFinalization {
    pub(super) fn new(session: Arc<Session>, completion: SessionTurnFinalization) -> Self {
        Self {
            session,
            completion: Some(completion),
        }
    }

    pub(super) fn turn_state(&self) -> &Arc<tokio::sync::Mutex<TurnState>> {
        let Some(completion) = self.completion.as_ref() else {
            unreachable!("pending finalization must retain its authority");
        };
        completion.turn_state()
    }

    pub(super) async fn complete(mut self) -> PendingFinalizationOutcome {
        let mut active_turn = self.session.active_turn.lock().await;
        let Some(completion) = self.completion.take() else {
            unreachable!("pending finalization must retain its authority");
        };
        let completion = active_turn.complete_finalization(completion);
        drop(active_turn);
        match completion {
            Ok(()) => PendingFinalizationOutcome::Completed,
            Err(completion) => {
                self.completion = Some(completion);
                PendingFinalizationOutcome::Displaced
            }
        }
    }
}

impl Drop for PendingFinalization {
    fn drop(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let recovery = PendingFinalizationRecovery {
            session: Arc::clone(&self.session),
            completion: Some(completion),
        };
        let runtime = self.session.services.runtime_handle.clone();
        let _recovery_task = runtime.spawn(recovery.poison());
    }
}

/// Detached poison recovery for finalization authority abandoned by its owner.
#[must_use = "finalization recovery must poison or displace its exact authority"]
struct PendingFinalizationRecovery {
    session: Arc<Session>,
    completion: Option<SessionTurnFinalization>,
}

enum PendingFinalizationRecoveryOutcome {
    Poisoned,
    Displaced,
}

impl PendingFinalizationRecovery {
    async fn poison(mut self) -> PendingFinalizationRecoveryOutcome {
        let mut active_turn = self.session.active_turn.lock().await;
        let Some(completion) = self.completion.take() else {
            unreachable!("finalization recovery must retain its authority");
        };
        let completion = active_turn.poison_abandoned_finalization(completion);
        drop(active_turn);
        match completion {
            Ok(()) => PendingFinalizationRecoveryOutcome::Poisoned,
            Err(completion) => {
                drop(completion);
                PendingFinalizationRecoveryOutcome::Displaced
            }
        }
    }
}

impl Drop for PendingFinalizationRecovery {
    fn drop(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let session = Arc::clone(&self.session);
        // This last resort must not depend on the session's Tokio runtime.
        // Dropping the authority after thread creation failure still signals
        // lifecycle completion while leaving the exact slot fail closed.
        if let Err(error) = std::thread::Builder::new()
            .name("codex-finalization-poison".to_string())
            .spawn(move || {
                let mut active_turn = session.active_turn.blocking_lock();
                let completion = active_turn.poison_abandoned_finalization(completion);
                drop(active_turn);
                drop(completion);
            })
        {
            tracing::error!(%error, "failed to start finalization poison fallback thread");
        }
    }
}

#[cfg(test)]
#[path = "finalization_tests.rs"]
mod tests;

use std::sync::Arc;
use std::sync::Weak;

use crate::session::session::Session;
use crate::state::TurnState;
use crate::state::SessionTurnFinalization;

#[must_use = "finalization must complete or poison its exact turn"]
pub(super) struct PendingFinalization {
    session: Weak<Session>,
    completion: Option<SessionTurnFinalization>,
}

impl PendingFinalization {
    pub(super) fn new(
        session: &Arc<Session>,
        completion: SessionTurnFinalization,
    ) -> Self {
        Self {
            session: Arc::downgrade(session),
            completion: Some(completion),
        }
    }

    pub(super) fn turn_state(&self) -> &Arc<tokio::sync::Mutex<TurnState>> {
        self.completion
            .as_ref()
            .expect("pending finalization must retain completion authority")
            .turn_state()
    }

    pub(super) async fn complete(mut self) -> bool {
        let Some(session) = self.session.upgrade() else {
            self.completion.take();
            return false;
        };
        let mut active_turn = session.active_turn.lock().await;
        let completion = self
            .completion
            .take()
            .expect("pending finalization must retain completion authority");
        match active_turn.complete_finalization(completion) {
            Ok(()) => true,
            Err(completion) => {
                self.completion = Some(completion);
                false
            }
        }
    }
}

impl Drop for PendingFinalization {
    fn drop(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(session) = self.session.upgrade() else {
            return;
        };
        let runtime = session.services.runtime_handle.clone();
        runtime.spawn(async move {
            session
                .active_turn
                .lock()
                .await
                .poison_abandoned_finalization(&completion);
        });
    }
}

#[cfg(test)]
#[path = "finalization_tests.rs"]
mod tests;

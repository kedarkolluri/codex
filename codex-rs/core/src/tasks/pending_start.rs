use std::sync::Arc;

use codex_protocol::protocol::TurnAbortReason;

use super::lifecycle::TurnStartLifecycleProgress;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
use crate::state::RunningTask;
use crate::state::SessionTurnSlot;
use crate::state::turn_lifecycle::TurnGeneration;
use crate::state::turn_lifecycle::TurnStartDriver;

/// Terminal result of compensating an uncommitted task start.
#[allow(dead_code)] // Activated by the atomic task-start stage.
#[derive(Debug, PartialEq)]
pub(super) enum PendingTaskStartOutcome {
    Cancelled(TurnAbortReason),
    Poisoned,
}

struct PendingTaskStartRecoveryState {
    driver: TurnStartDriver,
    lifecycle_progress: TurnStartLifecycleProgress,
    resolution: PendingTaskStartResolution,
}

#[derive(Clone, Copy)]
enum PendingTaskStartResolution {
    CompleteCancelled,
    Poison,
}

/// Linear authority for compensating one exact task start.
#[allow(dead_code)] // Activated by the atomic task-start stage.
#[must_use = "an exact task start must be compensated or poisoned"]
pub(super) struct PendingTaskStart {
    session: Arc<Session>,
    turn_context: Arc<TurnContext>,
    recovery_state: Option<PendingTaskStartRecoveryState>,
}

#[allow(dead_code)] // Activated by the atomic task-start stage.
impl PendingTaskStart {
    pub(super) fn new(
        session: Arc<Session>,
        turn_context: Arc<TurnContext>,
        driver: TurnStartDriver,
    ) -> Self {
        Self {
            session,
            turn_context,
            recovery_state: Some(PendingTaskStartRecoveryState {
                driver,
                lifecycle_progress: TurnStartLifecycleProgress::default(),
                resolution: PendingTaskStartResolution::CompleteCancelled,
            }),
        }
    }

    pub(super) fn generation(&self) -> TurnGeneration {
        let Some(recovery_state) = self.recovery_state.as_ref() else {
            unreachable!("pending start must retain its recovery authority");
        };
        recovery_state.driver.generation()
    }

    pub(super) fn lifecycle_progress_mut(&mut self) -> &mut TurnStartLifecycleProgress {
        let Some(recovery_state) = self.recovery_state.as_mut() else {
            unreachable!("pending start must retain its recovery authority");
        };
        &mut recovery_state.lifecycle_progress
    }

    pub(super) fn commit(
        mut self,
        active_turn: &mut SessionTurnSlot,
        task: RunningTask,
    ) -> Result<TurnStartLifecycleProgress, (Self, RunningTask)> {
        let Some(recovery_state) = self.recovery_state.take() else {
            unreachable!("pending start must retain its recovery authority");
        };
        let PendingTaskStartRecoveryState {
            driver,
            lifecycle_progress,
            resolution,
        } = recovery_state;
        match active_turn.commit_start(driver, task) {
            Ok(()) => Ok(lifecycle_progress),
            Err((driver, task)) => {
                self.recovery_state = Some(PendingTaskStartRecoveryState {
                    driver,
                    lifecycle_progress,
                    resolution,
                });
                Err((self, task))
            }
        }
    }

    pub(super) async fn compensate(mut self) -> PendingTaskStartOutcome {
        recover_pending_start(
            self.session.as_ref(),
            self.turn_context.as_ref(),
            &mut self.recovery_state,
        )
        .await
    }

    pub(super) async fn poison(mut self) -> PendingTaskStartOutcome {
        let Some(recovery_state) = self.recovery_state.as_mut() else {
            unreachable!("pending start must retain its recovery authority");
        };
        recovery_state.resolution = PendingTaskStartResolution::Poison;
        recover_pending_start(
            self.session.as_ref(),
            self.turn_context.as_ref(),
            &mut self.recovery_state,
        )
        .await
    }
}

impl Drop for PendingTaskStart {
    fn drop(&mut self) {
        let Some(recovery_state) = self.recovery_state.take() else {
            return;
        };
        let recovery = PendingTaskStartRecovery {
            session: Arc::clone(&self.session),
            turn_context: Arc::clone(&self.turn_context),
            recovery_state: Some(recovery_state),
        };
        let runtime = self.session.services.runtime_handle.clone();
        let _recovery_task = runtime.spawn(recovery.recover());
    }
}

/// Primary detached recovery. Dropping it starts one final poison-only fallback.
#[must_use = "start recovery must finish compensation or poison its generation"]
struct PendingTaskStartRecovery {
    session: Arc<Session>,
    turn_context: Arc<TurnContext>,
    recovery_state: Option<PendingTaskStartRecoveryState>,
}

impl PendingTaskStartRecovery {
    async fn recover(mut self) -> PendingTaskStartOutcome {
        recover_pending_start(
            self.session.as_ref(),
            self.turn_context.as_ref(),
            &mut self.recovery_state,
        )
        .await
    }
}

impl Drop for PendingTaskStartRecovery {
    fn drop(&mut self) {
        let Some(recovery_state) = self.recovery_state.take() else {
            return;
        };
        let session = Arc::clone(&self.session);
        // This last resort must not depend on the session's Tokio runtime.
        // If thread creation itself fails, dropping the captured driver still
        // publishes a poisoned terminal outcome and leaves the slot fail closed.
        if let Err(error) = std::thread::Builder::new()
            .name("codex-pending-start-poison".to_string())
            .spawn(move || {
                let PendingTaskStartRecoveryState {
                    driver,
                    lifecycle_progress: _,
                    resolution: _,
                } = recovery_state;
                let generation = driver.generation();
                let mut active_turn = session.active_turn.blocking_lock();
                if !active_turn.cancel_start_exact(&generation, TurnAbortReason::Interrupted) {
                    assert!(
                        generation.finished_outcome().is_some(),
                        "a displaced start driver must already have a terminal outcome"
                    );
                    drop(driver);
                    return;
                }
                assert!(
                    active_turn.poison_abandoned_start(driver).is_ok(),
                    "exact cancelled start must remain poisonable while its slot lock is held"
                );
            })
        {
            tracing::error!(%error, "failed to start pending-start poison fallback thread");
        }
    }
}

async fn recover_pending_start(
    session: &Session,
    turn_context: &TurnContext,
    recovery_state: &mut Option<PendingTaskStartRecoveryState>,
) -> PendingTaskStartOutcome {
    let generation = {
        let Some(recovery_state) = recovery_state.as_ref() else {
            unreachable!("start recovery must retain its authority");
        };
        recovery_state.driver.generation()
    };
    {
        let mut active_turn = session.active_turn.lock().await;
        if !active_turn.cancel_start_exact(&generation, TurnAbortReason::Interrupted) {
            assert!(
                generation.finished_outcome().is_some(),
                "a displaced start driver must already have a terminal outcome"
            );
            drop(recovery_state.take());
            return PendingTaskStartOutcome::Poisoned;
        }
    }
    let reason = generation
        .cancel_reason()
        .unwrap_or(TurnAbortReason::Interrupted);
    let Some(state) = recovery_state.as_mut() else {
        unreachable!("start recovery must retain its authority");
    };
    session
        .emit_entered_turn_abort_lifecycle(
            reason,
            turn_context.extension_data.as_ref(),
            &mut state.lifecycle_progress,
        )
        .await;

    let mut active_turn = session.active_turn.lock().await;
    let Some(recovery_state) = recovery_state.take() else {
        unreachable!("start recovery must retain its authority");
    };
    let PendingTaskStartRecoveryState {
        driver,
        lifecycle_progress: _,
        resolution,
    } = recovery_state;
    match resolution {
        PendingTaskStartResolution::CompleteCancelled => {
            match active_turn.complete_cancelled_start(driver) {
                Ok(reason) => PendingTaskStartOutcome::Cancelled(reason),
                Err(driver) => {
                    let generation = driver.generation();
                    let cancelled_exact =
                        active_turn.cancel_start_exact(&generation, TurnAbortReason::Interrupted);
                    if cancelled_exact {
                        assert!(
                            active_turn.poison_abandoned_start(driver).is_ok(),
                            "exact cancelled start must remain poisonable while its slot lock is held"
                        );
                    } else {
                        assert!(
                            generation.finished_outcome().is_some(),
                            "a displaced start driver must already have a terminal outcome"
                        );
                        drop(driver);
                    }
                    PendingTaskStartOutcome::Poisoned
                }
            }
        }
        PendingTaskStartResolution::Poison => {
            let generation = driver.generation();
            let cancelled_exact =
                active_turn.cancel_start_exact(&generation, TurnAbortReason::Interrupted);
            if cancelled_exact {
                if active_turn.poison_abandoned_start(driver).is_err() {
                    session.turn_start_gate.close();
                }
            } else {
                session.turn_start_gate.close();
                drop(driver);
            }
            PendingTaskStartOutcome::Poisoned
        }
    }
}

#[cfg(test)]
#[path = "pending_start_tests.rs"]
mod tests;

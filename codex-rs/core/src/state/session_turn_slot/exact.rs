use std::sync::Arc;

use codex_protocol::protocol::TurnAbortReason;
use tokio::sync::Mutex;

use crate::agent::control::AgentExecutionGuard;
use crate::session::turn_context::TurnContext;
use crate::state::RunningTask;
use crate::state::SessionTurnSlot;
use crate::state::TurnState;
use crate::state::turn_lifecycle::TurnFinalization as LifecycleFinalization;
use crate::state::turn_lifecycle::TurnGeneration;
use crate::state::turn_lifecycle::TurnStartDriver;

/// Exact completion authority and state for one finalizing session turn.
#[must_use = "finalization authority must be completed or poisoned"]
pub(crate) struct SessionTurnFinalization {
    turn_state: Arc<Mutex<TurnState>>,
    finalization: LifecycleFinalization,
}

impl SessionTurnFinalization {
    pub(crate) fn turn_state(&self) -> &Arc<Mutex<TurnState>> {
        &self.turn_state
    }
}

/// Running task plus the exact authority required to finish its lifecycle.
#[must_use = "a finalizing turn must be cleaned up and completed"]
pub(crate) struct FinalizingTurn {
    task: RunningTask,
    completion: SessionTurnFinalization,
}

impl FinalizingTurn {
    pub(crate) fn into_parts(self) -> (RunningTask, SessionTurnFinalization) {
        (self.task, self.completion)
    }
}

/// Exact state transition selected by a session-wide abort request.
#[must_use = "an abort transition must be driven to its lifecycle terminal"]
pub(crate) enum SessionTurnAbortTransition {
    Starting(TurnGeneration),
    Running(FinalizingTurn),
    Finalizing(TurnGeneration),
    Inactive,
}

impl SessionTurnSlot {
    pub(crate) fn can_begin_fresh_start(&self) -> bool {
        self.lifecycle.is_idle()
    }

    #[cfg(test)]
    pub(crate) fn can_begin_reserved_start(
        &self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        self.legacy_start.as_ref().is_some_and(|driver| {
            Arc::ptr_eq(driver.generation().turn_state(), expected_turn_state)
        })
    }

    pub(crate) fn begin_fresh_start(
        &mut self,
        execution_guard: Option<AgentExecutionGuard>,
    ) -> Result<TurnStartDriver, Option<AgentExecutionGuard>> {
        if !self.can_begin_fresh_start() {
            return Err(execution_guard);
        }
        self.lifecycle.start(execution_guard)
    }
    #[cfg(test)]
    pub(crate) fn begin_reserved_start(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
        execution_guard: Option<AgentExecutionGuard>,
    ) -> Result<TurnStartDriver, Option<AgentExecutionGuard>> {
        if !self.can_begin_reserved_start(expected_turn_state) {
            return Err(execution_guard);
        }
        let Some(driver) = self.legacy_start.take() else {
            unreachable!("reserved start was authenticated above");
        };
        match self.lifecycle.replace_start_lease(&driver, execution_guard) {
            Ok(previous_guard) => {
                debug_assert!(previous_guard.is_none());
                Ok(driver)
            }
            Err(execution_guard) => {
                self.legacy_start = Some(driver);
                Err(execution_guard)
            }
        }
    }
    pub(crate) fn cancel_start_exact(
        &mut self,
        generation: &TurnGeneration,
        reason: TurnAbortReason,
    ) -> bool {
        self.lifecycle.cancel_start_exact(generation, reason)
    }
    pub(crate) fn complete_cancelled_start(
        &mut self,
        driver: TurnStartDriver,
    ) -> Result<TurnAbortReason, TurnStartDriver> {
        self.lifecycle.complete_cancelled_start(driver)
    }
    pub(crate) fn poison_abandoned_start(
        &mut self,
        driver: TurnStartDriver,
    ) -> Result<(), TurnStartDriver> {
        self.lifecycle.poison_abandoned_start(driver)
    }
    pub(crate) fn commit_start(
        &mut self,
        driver: TurnStartDriver,
        task: RunningTask,
    ) -> Result<(), (TurnStartDriver, RunningTask)> {
        if task._agent_execution_guard.is_some() {
            return Err((driver, task));
        }
        self.lifecycle.commit_start(driver, task)
    }
    pub(crate) fn begin_abort(&mut self, reason: TurnAbortReason) -> SessionTurnAbortTransition {
        #[cfg(test)]
        {
            if let Some(driver) = self.legacy_start.take() {
                let generation = driver.generation();
                if !self
                    .lifecycle
                    .cancel_start_exact(&generation, reason.clone())
                {
                    self.legacy_start = Some(driver);
                    return SessionTurnAbortTransition::Inactive;
                }
                match self.lifecycle.complete_cancelled_start(driver) {
                    Ok(_) => return SessionTurnAbortTransition::Starting(generation),
                    Err(driver) => {
                        self.legacy_start = Some(driver);
                        return SessionTurnAbortTransition::Inactive;
                    }
                }
            }
        }
        if let Some(generation) = self.lifecycle.cancel_start(reason) {
            return SessionTurnAbortTransition::Starting(generation);
        }
        if let Some((generation, turn_context)) = self.running_identity()
            && let Some(turn) = self.begin_finalization(&generation, &turn_context)
        {
            return SessionTurnAbortTransition::Running(turn);
        }
        #[cfg(test)]
        let legacy_finalization_active = self.legacy_finalization.is_some();
        #[cfg(not(test))]
        let legacy_finalization_active = false;
        if !legacy_finalization_active
            && let Some(generation) = self.lifecycle.finalizing_generation().cloned()
        {
            return SessionTurnAbortTransition::Finalizing(generation);
        }
        SessionTurnAbortTransition::Inactive
    }
    pub(crate) fn begin_finalization(
        &mut self,
        generation: &TurnGeneration,
        turn_context: &Arc<TurnContext>,
    ) -> Option<FinalizingTurn> {
        #[cfg(test)]
        if self.legacy_running {
            return None;
        }
        let turn_state = Arc::clone(generation.turn_state());
        let (task, finalization) = self
            .lifecycle
            .begin_finalization(generation, turn_context)?;
        Some(FinalizingTurn {
            task,
            completion: SessionTurnFinalization {
                turn_state,
                finalization,
            },
        })
    }
    pub(crate) fn begin_running_finalization_for_turn(
        &mut self,
        turn_id: &str,
    ) -> Option<FinalizingTurn> {
        let (generation, turn_context) = self.running_identity()?;
        if turn_context.sub_id != turn_id {
            return None;
        }
        self.begin_finalization(&generation, &turn_context)
    }
    pub(crate) fn complete_finalization(
        &mut self,
        completion: SessionTurnFinalization,
    ) -> Result<(), SessionTurnFinalization> {
        let SessionTurnFinalization {
            turn_state,
            finalization,
        } = completion;
        match self.lifecycle.complete_finalization(finalization) {
            Ok(()) => Ok(()),
            Err(finalization) => Err(SessionTurnFinalization {
                turn_state,
                finalization,
            }),
        }
    }
    pub(crate) fn poison_abandoned_finalization(
        &mut self,
        completion: SessionTurnFinalization,
    ) -> Result<(), SessionTurnFinalization> {
        let SessionTurnFinalization {
            turn_state,
            finalization,
        } = completion;
        match self.lifecycle.poison_finalization(finalization) {
            Ok(()) => Ok(()),
            Err(finalization) => Err(SessionTurnFinalization {
                turn_state,
                finalization,
            }),
        }
    }
    fn running_identity(&self) -> Option<(TurnGeneration, Arc<TurnContext>)> {
        let (generation, turn_context, _task) = self.lifecycle.running()?;
        Some((generation.clone(), Arc::clone(turn_context)))
    }
}

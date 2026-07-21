use std::sync::Arc;

use codex_protocol::protocol::TurnAbortReason;
use tokio::sync::Mutex;

use crate::agent::control::AgentExecutionGuard;
use crate::session::turn_context::TurnContext;

use super::RunningTask;
use super::TurnState;
use super::turn_lifecycle::TurnFinalization as LifecycleFinalization;
use super::turn_lifecycle::TurnGeneration;
use super::turn_lifecycle::TurnLifecycleSlot;
use super::turn_lifecycle::TurnStartDriver;

type SessionLifecycle = TurnLifecycleSlot<Option<AgentExecutionGuard>, RunningTask>;

/// Session-owned exact lifecycle for the single active task slot.
#[derive(Default)]
pub(crate) struct SessionTurnSlot {
    lifecycle: SessionLifecycle,
    reserved_start: Option<TurnStartDriver>,
}

/// Borrowed view of a turn that has an installed running task.
pub(crate) struct RunningTurnRef<'a> {
    task: &'a RunningTask,
    turn_state: &'a Arc<Mutex<TurnState>>,
}

impl RunningTurnRef<'_> {
    pub(crate) fn task(&self) -> &RunningTask {
        self.task
    }

    pub(crate) fn turn_state(&self) -> &Arc<Mutex<TurnState>> {
        self.turn_state
    }
}

/// Exact completion authority and state for one finalizing session turn.
pub(crate) struct SessionTurnFinalization {
    generation: TurnGeneration,
    turn_context: Arc<TurnContext>,
    turn_state: Arc<Mutex<TurnState>>,
    finalization: LifecycleFinalization,
}

impl SessionTurnFinalization {
    pub(crate) fn generation(&self) -> &TurnGeneration {
        &self.generation
    }

    pub(crate) fn turn_context(&self) -> &Arc<TurnContext> {
        &self.turn_context
    }

    pub(crate) fn turn_state(&self) -> &Arc<Mutex<TurnState>> {
        &self.turn_state
    }
}

pub(crate) struct FinalizingTurn {
    task: RunningTask,
    completion: SessionTurnFinalization,
}

impl FinalizingTurn {
    pub(crate) fn into_parts(self) -> (RunningTask, SessionTurnFinalization) {
        (self.task, self.completion)
    }
}

pub(crate) enum SessionTurnAbortTransition {
    Starting(TurnGeneration),
    Running(FinalizingTurn),
    Finalizing(TurnGeneration),
    Inactive,
}

impl SessionTurnSlot {
    pub(crate) fn is_idle(&self) -> bool {
        self.lifecycle.is_idle()
    }

    pub(crate) fn has_active_turn(&self) -> bool {
        !self.is_idle()
    }

    pub(crate) fn can_begin_fresh_start(&self) -> bool {
        self.lifecycle.is_idle()
    }

    pub(crate) fn can_begin_reserved_start(
        &self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        self.reserved_start.as_ref().is_some_and(|driver| {
            Arc::ptr_eq(driver.generation().turn_state(), expected_turn_state)
        })
    }

    pub(crate) fn current_turn_state(&self) -> Option<&Arc<Mutex<TurnState>>> {
        self.lifecycle
            .current_generation()
            .map(TurnGeneration::turn_state)
    }

    #[cfg(test)]
    pub(crate) fn current_generation(&self) -> Option<TurnGeneration> {
        self.lifecycle.current_generation().cloned()
    }

    pub(crate) fn running_turn(&self) -> Option<RunningTurnRef<'_>> {
        let (generation, task) = self.lifecycle.running()?;
        Some(RunningTurnRef {
            task,
            turn_state: generation.turn_state(),
        })
    }

    pub(crate) fn finalizing_generation(&self) -> Option<TurnGeneration> {
        self.lifecycle.finalizing_generation().cloned()
    }

    #[cfg(test)]
    pub(crate) fn running_generation(&self) -> Option<TurnGeneration> {
        self.lifecycle
            .running()
            .map(|(generation, _task)| generation.clone())
    }

    /// Compatibility reservation for callers that prepare exact turn state before task start.
    pub(crate) fn reserve_taskless(&mut self) -> Option<&Arc<Mutex<TurnState>>> {
        if self.reserved_start.is_none() {
            let driver = self.lifecycle.start(/*lease*/ None).ok()?;
            self.reserved_start = Some(driver);
        }
        self.current_turn_state()
    }

    pub(crate) fn clear_taskless_exact_state(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        let Some(driver) = self.reserved_start.as_ref() else {
            return false;
        };
        if !Arc::ptr_eq(driver.generation().turn_state(), expected_turn_state) {
            return false;
        }
        let driver = self
            .reserved_start
            .take()
            .expect("reserved start was authenticated above");
        let Some(_) = self.lifecycle.cancel_start(TurnAbortReason::Replaced) else {
            unreachable!("reserved start must remain in Starting");
        };
        let Ok(_) = self.lifecycle.complete_cancelled_start(driver) else {
            unreachable!("reserved start driver must compensate its generation");
        };
        true
    }

    pub(crate) fn begin_fresh_start(
        &mut self,
        lease: Option<AgentExecutionGuard>,
    ) -> Result<TurnStartDriver, Option<AgentExecutionGuard>> {
        if !self.can_begin_fresh_start() {
            return Err(lease);
        }
        self.lifecycle.start(lease)
    }

    pub(crate) fn begin_reserved_start(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
        lease: Option<AgentExecutionGuard>,
    ) -> Result<TurnStartDriver, Option<AgentExecutionGuard>> {
        if !self.can_begin_reserved_start(expected_turn_state) {
            return Err(lease);
        }
        let driver = self
            .reserved_start
            .take()
            .expect("reserved start was authenticated above");
        match self.lifecycle.replace_start_lease(&driver, lease) {
            Ok(previous_lease) => {
                debug_assert!(previous_lease.is_none());
                Ok(driver)
            }
            Err(lease) => {
                self.reserved_start = Some(driver);
                Err(lease)
            }
        }
    }

    pub(crate) fn cancel_start(&mut self, reason: TurnAbortReason) -> Option<TurnGeneration> {
        let generation = self.lifecycle.cancel_start(reason)?;
        if let Some(driver) = self.reserved_start.take() {
            let Ok(_) = self.lifecycle.complete_cancelled_start(driver) else {
                unreachable!("reserved start driver must compensate its generation");
            };
        }
        Some(generation)
    }

    pub(crate) fn cancel_start_exact(
        &mut self,
        generation: &TurnGeneration,
        reason: TurnAbortReason,
    ) -> bool {
        self.lifecycle.cancel_start_exact(generation, reason)
    }

    pub(crate) fn begin_abort(
        &mut self,
        reason: TurnAbortReason,
    ) -> SessionTurnAbortTransition {
        if let Some(generation) = self.cancel_start(reason) {
            return SessionTurnAbortTransition::Starting(generation);
        }
        if let Some(turn) = self.begin_running_finalization() {
            return SessionTurnAbortTransition::Running(turn);
        }
        if let Some(generation) = self.finalizing_generation() {
            return SessionTurnAbortTransition::Finalizing(generation);
        }
        SessionTurnAbortTransition::Inactive
    }

    pub(crate) fn complete_cancelled_start(
        &mut self,
        driver: TurnStartDriver,
    ) -> Result<TurnAbortReason, TurnStartDriver> {
        self.lifecycle.complete_cancelled_start(driver)
    }

    pub(crate) fn poison_abandoned_start(&mut self, generation: &TurnGeneration) -> bool {
        self.lifecycle.poison_abandoned_start(generation)
    }

    pub(crate) fn commit_start(
        &mut self,
        driver: TurnStartDriver,
        task: RunningTask,
    ) -> Result<(), (TurnStartDriver, RunningTask)> {
        self.lifecycle.commit_start(driver, task)
    }

    pub(crate) fn begin_finalization(
        &mut self,
        generation: &TurnGeneration,
        turn_context: &Arc<TurnContext>,
    ) -> Option<FinalizingTurn> {
        let turn_state = Arc::clone(generation.turn_state());
        let (task, finalization) = self
            .lifecycle
            .begin_finalization(generation, turn_context)?;
        Some(FinalizingTurn {
            task,
            completion: SessionTurnFinalization {
                generation: generation.clone(),
                turn_context: Arc::clone(turn_context),
                turn_state,
                finalization,
            },
        })
    }

    pub(crate) fn begin_running_finalization(&mut self) -> Option<FinalizingTurn> {
        let (generation, turn_context) = self.running_identity()?;
        self.begin_finalization(&generation, &turn_context)
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
            generation,
            turn_context,
            turn_state,
            finalization,
        } = completion;
        match self.lifecycle.complete_finalization(finalization) {
            Ok(()) => Ok(()),
            Err(finalization) => Err(SessionTurnFinalization {
                generation,
                turn_context,
                turn_state,
                finalization,
            }),
        }
    }

    pub(crate) fn poison_abandoned_finalization(
        &mut self,
        completion: &SessionTurnFinalization,
    ) -> bool {
        self.lifecycle.poison_finalization_for_turn(
            completion.generation(),
            completion.turn_context(),
        )
    }

    fn running_identity(&self) -> Option<(TurnGeneration, Arc<TurnContext>)> {
        let (generation, task) = self.lifecycle.running()?;
        Some((generation.clone(), Arc::clone(&task.turn_context)))
    }
}

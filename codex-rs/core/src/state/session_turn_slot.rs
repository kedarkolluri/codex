use std::sync::Arc;

#[cfg(test)]
use codex_protocol::protocol::TurnAbortReason;
use tokio::sync::Mutex;

use crate::agent::control::AgentExecutionGuard;

#[cfg(test)]
use super::ActiveTurn;
use super::RunningTask;
use super::TurnState;
use super::turn_lifecycle::TurnFinalization;
use super::turn_lifecycle::TurnGeneration;
use super::turn_lifecycle::TurnLifecycleSlot;
use super::turn_lifecycle::TurnStartDriver;

mod exact;

pub(crate) use exact::SessionTurnAbortTransition;
pub(crate) use exact::SessionTurnFinalization;

type SessionLifecycle = TurnLifecycleSlot<Option<AgentExecutionGuard>, RunningTask>;

/// Session-owned compatibility adapter over the exact turn lifecycle slot.
///
/// The stored linear authorities preserve the legacy two-phase start and
/// finish APIs until their callers move to exact lifecycle transactions.
#[derive(Default)]
pub(crate) struct SessionTurnSlot {
    lifecycle: SessionLifecycle,
    legacy_start: Option<TurnStartDriver>,
    legacy_running: bool,
    legacy_finalization: Option<LegacyFinalization>,
}

#[cfg_attr(not(test), allow(dead_code))]
struct LegacyFinalization {
    authority: TurnFinalization,
    turn_state: Arc<Mutex<TurnState>>,
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

impl SessionTurnSlot {
    pub(crate) fn is_idle(&self) -> bool {
        self.lifecycle.is_idle()
    }

    pub(crate) fn has_active_turn(&self) -> bool {
        !self.is_idle()
    }

    pub(crate) fn current_generation(&self) -> Option<&TurnGeneration> {
        self.lifecycle.current_generation()
    }

    pub(crate) fn current_turn_state(&self) -> Option<&Arc<Mutex<TurnState>>> {
        self.current_generation().map(TurnGeneration::turn_state)
    }

    pub(crate) fn starting_generation(&self) -> Option<TurnGeneration> {
        self.lifecycle.starting_generation().cloned()
    }

    pub(crate) fn running_turn(&self) -> Option<RunningTurnRef<'_>> {
        let (generation, _turn_context, task) = self.lifecycle.running()?;
        Some(RunningTurnRef {
            task,
            turn_state: generation.turn_state(),
        })
    }

    /// Test-only legacy taskless reservation.
    #[cfg(test)]
    pub(crate) fn reserve_taskless(&mut self) -> Option<&Arc<Mutex<TurnState>>> {
        if self.lifecycle.running().is_some() {
            return None;
        }
        if self.legacy_start.is_some() || self.legacy_finalization.is_some() {
            return self.current_turn_state();
        }
        if !self.lifecycle.is_idle() {
            return None;
        }
        let driver = self.lifecycle.start(/*lease*/ None).ok()?;
        self.legacy_start = Some(driver);
        self.current_turn_state()
    }

    /// Test-only legacy first get-or-insert projection.
    #[cfg(test)]
    pub(crate) fn reserve_taskless_for_legacy_start(&mut self) -> &Arc<Mutex<TurnState>> {
        let Some(turn_state) = self.reserve_taskless() else {
            unreachable!("legacy start must not overlap an exact lifecycle owner");
        };
        turn_state
    }

    /// Test-only legacy second get-or-insert and running-task assignment.
    #[cfg(test)]
    pub(crate) fn install_running_task_for_legacy_start(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
        mut task: RunningTask,
    ) {
        if !self
            .current_turn_state()
            .is_some_and(|turn_state| Arc::ptr_eq(turn_state, expected_turn_state))
        {
            return;
        }
        let Some(driver) = self.legacy_start.take() else {
            return;
        };
        let execution_guard = task._agent_execution_guard.take();
        let previous_guard = match self.lifecycle.replace_start_lease(&driver, execution_guard) {
            Ok(previous_guard) => previous_guard,
            Err(execution_guard) => {
                task._agent_execution_guard = execution_guard;
                self.legacy_start = Some(driver);
                return;
            }
        };
        drop(previous_guard);
        match self.lifecycle.commit_start(driver, task) {
            Ok(()) => self.legacy_running = true,
            Err((driver, _task)) => {
                self.legacy_start = Some(driver);
                debug_assert!(false, "legacy task install must commit its reservation");
            }
        }
    }

    /// Test-only unconditional legacy abort take.
    #[cfg(test)]
    pub(crate) fn take_for_legacy_abort(&mut self) -> Option<ActiveTurn> {
        if let Some((task, turn_state)) = self.take_running_for_legacy_removal() {
            return Some(ActiveTurn::from_parts(Some(task), turn_state));
        }

        if let Some(driver) = self.legacy_start.take() {
            let generation = driver.generation();
            let turn_state = Arc::clone(generation.turn_state());
            let Some(_) = self.lifecycle.cancel_start(TurnAbortReason::Replaced) else {
                self.legacy_start = Some(driver);
                return None;
            };
            let Ok(_) = self.lifecycle.complete_cancelled_start(driver) else {
                unreachable!("stored legacy start authority must be exact");
            };
            return Some(ActiveTurn::from_parts(/*task*/ None, turn_state));
        }

        let finalization = self.legacy_finalization.take()?;
        let turn_state = finalization.turn_state;
        let Ok(()) = self.lifecycle.complete_finalization(finalization.authority) else {
            unreachable!("stored legacy finalization authority must be exact");
        };
        Some(ActiveTurn::from_parts(/*task*/ None, turn_state))
    }

    /// Test-only legacy finish projection.
    #[cfg(test)]
    pub(crate) fn take_running_task_for_legacy_finish(
        &mut self,
    ) -> Option<(RunningTask, Arc<Mutex<TurnState>>)> {
        if !self.legacy_running || self.legacy_finalization.is_some() {
            return None;
        }
        let (generation, turn_context, _task) = self.lifecycle.running()?;
        let generation = generation.clone();
        let turn_context = Arc::clone(turn_context);
        let turn_state = Arc::clone(generation.turn_state());
        let (task, authority) = self
            .lifecycle
            .begin_finalization(&generation, &turn_context)?;
        self.legacy_running = false;
        self.legacy_finalization = Some(LegacyFinalization {
            authority,
            turn_state: Arc::clone(&turn_state),
        });
        Some((task, turn_state))
    }

    /// Test-only exact taskless cleanup.
    #[cfg(test)]
    pub(crate) fn clear_taskless_exact_state(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        self.clear_taskless_exact_state_inner(expected_turn_state)
    }

    /// Test-only exact legacy finish cleanup.
    #[cfg(test)]
    pub(crate) fn clear_legacy_finished_exact_state(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        self.clear_taskless_exact_state_inner(expected_turn_state)
    }

    #[cfg(test)]
    fn take_running_for_legacy_removal(&mut self) -> Option<(RunningTask, Arc<Mutex<TurnState>>)> {
        if !self.legacy_running {
            return None;
        }
        let (generation, turn_context, _task) = self.lifecycle.running()?;
        let generation = generation.clone();
        let turn_context = Arc::clone(turn_context);
        let turn_state = Arc::clone(generation.turn_state());
        let (task, authority) = self
            .lifecycle
            .begin_finalization(&generation, &turn_context)?;
        self.legacy_running = false;
        let Ok(()) = self.lifecycle.complete_finalization(authority) else {
            unreachable!("fresh legacy removal authority must be exact");
        };
        Some((task, turn_state))
    }

    #[cfg(test)]
    fn clear_taskless_exact_state_inner(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        if !self
            .current_turn_state()
            .is_some_and(|turn_state| Arc::ptr_eq(turn_state, expected_turn_state))
            || self.lifecycle.running().is_some()
        {
            return false;
        }

        if let Some(driver) = self.legacy_start.take() {
            let Some(_) = self.lifecycle.cancel_start(TurnAbortReason::Replaced) else {
                self.legacy_start = Some(driver);
                return false;
            };
            return match self.lifecycle.complete_cancelled_start(driver) {
                Ok(_) => true,
                Err(driver) => {
                    self.legacy_start = Some(driver);
                    false
                }
            };
        }

        let Some(finalization) = self.legacy_finalization.take() else {
            return false;
        };
        match self.lifecycle.complete_finalization(finalization.authority) {
            Ok(()) => true,
            Err(authority) => {
                self.legacy_finalization = Some(LegacyFinalization {
                    authority,
                    turn_state: finalization.turn_state,
                });
                false
            }
        }
    }
}

#[cfg(test)]
#[path = "session_turn_slot_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "session_turn_slot/exact_tests.rs"]
mod exact_tests;

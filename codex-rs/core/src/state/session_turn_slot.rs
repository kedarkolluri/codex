use std::sync::Arc;

use tokio::sync::Mutex;

use super::ActiveTurn;
use super::RunningTask;
use super::TurnState;

/// Session-owned compatibility slot for the legacy active-turn representation.
///
/// Keeping reads phase-shaped prevents callers from borrowing the inner option
/// independently of the session's outer mutex guard. The next task-lifecycle
/// activation replaces the representation and removes the legacy mutations.
#[derive(Default)]
pub(crate) struct SessionTurnSlot {
    active_turn: Option<ActiveTurn>,
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
        self.active_turn.is_none()
    }

    pub(crate) fn has_active_turn(&self) -> bool {
        self.active_turn.is_some()
    }

    pub(crate) fn current_turn_state(&self) -> Option<&Arc<Mutex<TurnState>>> {
        self.active_turn.as_ref().map(ActiveTurn::turn_state)
    }

    pub(crate) fn running_turn(&self) -> Option<RunningTurnRef<'_>> {
        let active_turn = self.active_turn.as_ref()?;
        Some(RunningTurnRef {
            task: active_turn.running_task()?,
            turn_state: active_turn.turn_state(),
        })
    }

    /// Legacy taskless reservation; removed by the next lifecycle activation.
    pub(crate) fn reserve_taskless(&mut self) -> Option<&Arc<Mutex<TurnState>>> {
        if self
            .active_turn
            .as_ref()
            .is_some_and(|active_turn| active_turn.running_task().is_some())
        {
            return None;
        }
        Some(
            self.active_turn
                .get_or_insert_with(ActiveTurn::default)
                .turn_state(),
        )
    }

    /// Legacy first get-or-insert and debug-only running-task check.
    ///
    /// This intentionally preserves the old release-build behavior until the
    /// next lifecycle activation makes admission reject a running slot.
    pub(crate) fn reserve_taskless_for_legacy_start(&mut self) -> &Arc<Mutex<TurnState>> {
        let active_turn = self.active_turn.get_or_insert_with(ActiveTurn::default);
        debug_assert!(active_turn.running_task().is_none());
        active_turn.turn_state()
    }

    /// Legacy second get-or-insert and running-task assignment.
    ///
    /// This intentionally preserves the old unchecked install semantics until
    /// the next lifecycle activation makes reservation and installation one
    /// exact transaction.
    pub(crate) fn install_running_task_for_legacy_start(&mut self, task: RunningTask) {
        let active_turn = self.active_turn.get_or_insert_with(ActiveTurn::default);
        debug_assert!(active_turn.running_task().is_none());
        active_turn.install_running_task(task);
    }

    /// Unconditional legacy abort take; removed by the next lifecycle activation.
    pub(crate) fn take_for_legacy_abort(&mut self) -> Option<ActiveTurn> {
        self.active_turn.take()
    }

    /// Exact legacy running-turn abort take; removed by the next lifecycle activation.
    pub(crate) fn take_running_turn_for_abort(&mut self, turn_id: &str) -> Option<ActiveTurn> {
        self.active_turn
            .as_ref()
            .and_then(ActiveTurn::running_task)
            .is_some_and(|task| task.turn_context.sub_id == turn_id)
            .then(|| self.active_turn.take())
            .flatten()
    }

    /// Legacy finish projection; removed by the next lifecycle activation.
    pub(crate) fn take_running_task_for_legacy_finish(
        &mut self,
    ) -> Option<(RunningTask, Arc<Mutex<TurnState>>)> {
        let active_turn = self.active_turn.as_mut()?;
        let task = active_turn.take_running_task()?;
        Some((task, Arc::clone(active_turn.turn_state())))
    }

    /// Exact taskless cleanup; removed by the next lifecycle activation.
    pub(crate) fn clear_taskless_exact_state(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        self.clear_taskless_exact_state_inner(expected_turn_state)
    }

    /// Exact legacy finish cleanup; removed by the next lifecycle activation.
    pub(crate) fn clear_legacy_finished_exact_state(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        self.clear_taskless_exact_state_inner(expected_turn_state)
    }

    fn clear_taskless_exact_state_inner(
        &mut self,
        expected_turn_state: &Arc<Mutex<TurnState>>,
    ) -> bool {
        let should_clear = self.active_turn.as_ref().is_some_and(|active_turn| {
            active_turn.running_task().is_none()
                && Arc::ptr_eq(active_turn.turn_state(), expected_turn_state)
        });
        if should_clear {
            self.active_turn = None;
        }
        should_clear
    }
}

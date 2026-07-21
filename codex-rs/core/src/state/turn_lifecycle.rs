//! Exact-identity state transitions for the single task slot owned by a session.
use crate::session::turn_context::TurnContext;
use crate::state::TurnState;
use codex_protocol::protocol::TurnAbortReason;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
/// Exact identity for one admission to the task slot.
#[derive(Clone)]
pub(crate) struct TurnGeneration {
    token: Arc<()>,
    turn_state: Arc<Mutex<TurnState>>,
    start_control: Arc<TurnStartControl>,
    lifecycle_finished: CancellationToken,
}
impl TurnGeneration {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.token, &other.token)
    }

    pub(crate) fn turn_state(&self) -> &Arc<Mutex<TurnState>> {
        &self.turn_state
    }

    pub(crate) fn cancel_reason(&self) -> Option<TurnAbortReason> {
        self.start_control.cancel_reason()
    }

    pub(crate) async fn cancelled(&self) -> TurnAbortReason {
        self.start_control.cancellation.cancelled().await;
        let Some(reason) = self.start_control.cancel_reason() else {
            unreachable!("cancelled task start must retain its reason");
        };
        reason
    }

    pub(crate) async fn wait_finished(&self) -> TurnStartOutcome {
        self.start_control.wait_finished().await
    }

    pub(crate) fn finished_outcome(&self) -> Option<TurnStartOutcome> {
        self.start_control.outcome()
    }

    pub(crate) async fn wait_lifecycle_finished(&self) {
        self.lifecycle_finished.cancelled().await;
    }

    fn finish_lifecycle(&self) {
        self.lifecycle_finished.cancel();
    }
}
/// Linear authority that alone may commit or compensate a starting turn.
#[must_use = "a task-start driver must commit, compensate, or be poisoned"]
pub(crate) struct TurnStartDriver {
    generation: TurnGeneration,
}
impl TurnStartDriver {
    pub(crate) fn generation(&self) -> TurnGeneration {
        self.generation.clone()
    }
}
impl Drop for TurnStartDriver {
    fn drop(&mut self) {
        if self.generation.finished_outcome().is_some() {
            return;
        }
        self.generation
            .start_control
            .request_cancel(TurnAbortReason::Interrupted);
        let Some(reason) = self.generation.start_control.cancel_reason() else {
            unreachable!("abandoned task start must retain its cancellation reason");
        };
        if self
            .generation
            .start_control
            .finish_if_unfinished(TurnStartOutcome::Poisoned(reason))
        {
            self.generation.finish_lifecycle();
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum TurnStartOutcome {
    Committed,
    Cancelled(TurnAbortReason),
    Poisoned(TurnAbortReason),
}
struct TurnStartControl {
    cancel_reason: StdMutex<Option<TurnAbortReason>>,
    cancellation: CancellationToken,
    outcome_tx: watch::Sender<Option<TurnStartOutcome>>,
}
impl TurnStartControl {
    fn new() -> Self {
        let (outcome_tx, _outcome_rx) = watch::channel(None);
        Self {
            cancel_reason: StdMutex::new(None),
            cancellation: CancellationToken::new(),
            outcome_tx,
        }
    }

    fn request_cancel(&self, reason: TurnAbortReason) {
        let mut cancel_reason = match self.cancel_reason.lock() {
            Ok(cancel_reason) => cancel_reason,
            Err(poisoned) => poisoned.into_inner(),
        };
        if cancel_reason.is_none() {
            *cancel_reason = Some(reason);
            self.cancellation.cancel();
        }
    }

    fn cancel_reason(&self) -> Option<TurnAbortReason> {
        match self.cancel_reason.lock() {
            Ok(cancel_reason) => cancel_reason.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn finish(&self, outcome: TurnStartOutcome) {
        assert!(
            self.finish_if_unfinished(outcome),
            "task start finished twice"
        );
    }

    fn finish_if_unfinished(&self, outcome: TurnStartOutcome) -> bool {
        self.outcome_tx.send_if_modified(|current| {
            if current.is_some() {
                false
            } else {
                *current = Some(outcome);
                true
            }
        })
    }

    async fn wait_finished(&self) -> TurnStartOutcome {
        let mut outcome_rx = self.outcome_tx.subscribe();
        loop {
            if let Some(outcome) = outcome_rx.borrow_and_update().clone() {
                return outcome;
            }
            let Ok(()) = outcome_rx.changed().await else {
                unreachable!("task-start control owns the watch sender");
            };
        }
    }

    fn outcome(&self) -> Option<TurnStartOutcome> {
        self.outcome_tx.borrow().clone()
    }
}
struct TurnOwner {
    generation: TurnGeneration,
    turn_context: Arc<TurnContext>,
}
impl TurnOwner {
    fn matches(&self, generation: &TurnGeneration, turn_context: &Arc<TurnContext>) -> bool {
        self.generation.matches(generation) && Arc::ptr_eq(&self.turn_context, turn_context)
    }
}
/// Exact authority to finish or poison one finalizing generation.
#[must_use = "a finalizing turn must be completed or poisoned"]
pub(crate) struct TurnFinalization {
    token: Arc<AtomicBool>,
    generation: TurnGeneration,
}

impl Drop for TurnFinalization {
    fn drop(&mut self) {
        if self
            .token
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.generation.finish_lifecycle();
        }
    }
}
enum TurnLifecycleState<L, R> {
    Idle,
    Starting {
        generation: TurnGeneration,
        lease: L,
    },
    Running {
        owner: TurnOwner,
        lease: L,
        task: R,
    },
    Finalizing {
        owner: TurnOwner,
        lease: L,
        token: Arc<AtomicBool>,
    },
    Poisoned {
        _generation: TurnGeneration,
        _turn_context: Option<Arc<TurnContext>>,
        _lease: L,
    },
}
/// Exhaustive transaction state for one session-owned task slot.
///
/// `L` is a lifecycle lease, such as reserved execution capacity, that remains
/// owned until successful finalization and remains held forever if the slot is
/// poisoned. `R` is the running task payload removed at finalization.
pub(crate) struct TurnLifecycleSlot<L, R> {
    state: TurnLifecycleState<L, R>,
}
impl<L, R> Default for TurnLifecycleSlot<L, R> {
    fn default() -> Self {
        Self {
            state: TurnLifecycleState::Idle,
        }
    }
}
/// A running payload whose exact turn context authenticates lifecycle transitions.
///
/// Implementations must return the same shared context used by all task execution,
/// completion, and abort paths for the payload.
pub(crate) trait TurnLifecycleTask {
    fn turn_context(&self) -> &Arc<TurnContext>;
}
impl<L, R> TurnLifecycleSlot<L, R> {
    pub(crate) fn is_idle(&self) -> bool {
        matches!(self.state, TurnLifecycleState::Idle)
    }

    pub(crate) fn current_generation(&self) -> Option<&TurnGeneration> {
        match &self.state {
            TurnLifecycleState::Idle => None,
            TurnLifecycleState::Starting { generation, .. }
            | TurnLifecycleState::Poisoned {
                _generation: generation,
                ..
            } => Some(generation),
            TurnLifecycleState::Running { owner, .. }
            | TurnLifecycleState::Finalizing { owner, .. } => Some(&owner.generation),
        }
    }

    #[allow(dead_code)] // Activated by the atomic task-start stage.
    pub(crate) fn starting_generation(&self) -> Option<&TurnGeneration> {
        let TurnLifecycleState::Starting { generation, .. } = &self.state else {
            return None;
        };
        Some(generation)
    }

    pub(crate) fn running(&self) -> Option<(&TurnGeneration, &Arc<TurnContext>, &R)> {
        let TurnLifecycleState::Running { owner, task, .. } = &self.state else {
            return None;
        };
        Some((&owner.generation, &owner.turn_context, task))
    }

    pub(crate) fn finalizing_generation(&self) -> Option<&TurnGeneration> {
        let TurnLifecycleState::Finalizing { owner, .. } = &self.state else {
            return None;
        };
        Some(&owner.generation)
    }

    /// Preserves the legacy unchecked task assignment while callers migrate.
    ///
    /// A running task is replaced in the same generation, and a taskless
    /// finalization is reopened as running. The latter invalidates its exact
    /// finalization authority, matching the legacy state check that prevents
    /// the old cleanup from clearing the newly installed task.
    pub(crate) fn install_running_task_for_legacy(&mut self, task: R) -> Result<Option<R>, R>
    where
        R: TurnLifecycleTask,
    {
        let turn_context = Arc::clone(task.turn_context());
        if let TurnLifecycleState::Running {
            owner,
            task: running_task,
            ..
        } = &mut self.state
        {
            owner.turn_context = turn_context;
            return Ok(Some(std::mem::replace(running_task, task)));
        }
        let TurnLifecycleState::Finalizing { token, .. } = &self.state else {
            return Err(task);
        };
        if token
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(task);
        }
        let TurnLifecycleState::Finalizing {
            mut owner, lease, ..
        } = std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("finalizing state was checked before replacement");
        };
        owner.turn_context = turn_context;
        self.state = TurnLifecycleState::Running { owner, lease, task };
        Ok(None)
    }

    pub(crate) fn start(&mut self, lease: L) -> Result<TurnStartDriver, L> {
        if !matches!(self.state, TurnLifecycleState::Idle) {
            return Err(lease);
        }
        let generation = TurnGeneration {
            token: Arc::new(()),
            turn_state: Arc::new(Mutex::new(TurnState::default())),
            start_control: Arc::new(TurnStartControl::new()),
            lifecycle_finished: CancellationToken::new(),
        };
        self.state = TurnLifecycleState::Starting {
            generation: generation.clone(),
            lease,
        };
        Ok(TurnStartDriver { generation })
    }

    pub(crate) fn replace_start_lease(
        &mut self,
        driver: &TurnStartDriver,
        lease: L,
    ) -> Result<L, L> {
        let TurnLifecycleState::Starting {
            generation,
            lease: active_lease,
        } = &mut self.state
        else {
            return Err(lease);
        };
        if !generation.matches(&driver.generation) {
            return Err(lease);
        }
        Ok(std::mem::replace(active_lease, lease))
    }

    pub(crate) fn cancel_start(&mut self, reason: TurnAbortReason) -> Option<TurnGeneration> {
        let TurnLifecycleState::Starting { generation, .. } = &mut self.state else {
            return None;
        };
        generation.start_control.request_cancel(reason);
        Some(generation.clone())
    }

    pub(crate) fn cancel_start_exact(
        &mut self,
        generation: &TurnGeneration,
        reason: TurnAbortReason,
    ) -> bool {
        let TurnLifecycleState::Starting {
            generation: active_generation,
            ..
        } = &mut self.state
        else {
            return false;
        };
        if !active_generation.matches(generation) {
            return false;
        }
        active_generation.start_control.request_cancel(reason);
        true
    }

    pub(crate) fn complete_cancelled_start(
        &mut self,
        driver: TurnStartDriver,
    ) -> Result<TurnAbortReason, TurnStartDriver> {
        let TurnLifecycleState::Starting {
            generation: active_generation,
            ..
        } = &self.state
        else {
            return Err(driver);
        };
        if !active_generation.matches(&driver.generation) {
            return Err(driver);
        }
        let Some(reason) = active_generation.start_control.cancel_reason() else {
            return Err(driver);
        };
        let TurnLifecycleState::Starting { lease, .. } =
            std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("starting state was authenticated before replacement");
        };
        drop(lease);
        driver
            .generation
            .start_control
            .finish(TurnStartOutcome::Cancelled(reason.clone()));
        driver.generation.finish_lifecycle();
        Ok(reason)
    }

    pub(crate) fn commit_start(
        &mut self,
        driver: TurnStartDriver,
        task: R,
    ) -> Result<(), (TurnStartDriver, R)>
    where
        R: TurnLifecycleTask,
    {
        let turn_context = Arc::clone(task.turn_context());
        let TurnLifecycleState::Starting {
            generation: active_generation,
            ..
        } = &self.state
        else {
            return Err((driver, task));
        };
        if !active_generation.matches(&driver.generation) {
            return Err((driver, task));
        }
        if active_generation.start_control.cancel_reason().is_some() {
            return Err((driver, task));
        }
        let TurnLifecycleState::Starting {
            generation, lease, ..
        } = std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("starting state was authenticated before replacement");
        };
        let start_control = Arc::clone(&generation.start_control);
        self.state = TurnLifecycleState::Running {
            owner: TurnOwner {
                generation,
                turn_context,
            },
            lease,
            task,
        };
        start_control.finish(TurnStartOutcome::Committed);
        Ok(())
    }

    /// Poisons an exact cancelled start while consuming its linear driver.
    pub(crate) fn poison_abandoned_start(
        &mut self,
        driver: TurnStartDriver,
    ) -> Result<(), TurnStartDriver> {
        let TurnLifecycleState::Starting {
            generation: active_generation,
            ..
        } = &self.state
        else {
            return Err(driver);
        };
        if !active_generation.matches(&driver.generation) {
            return Err(driver);
        }
        let Some(reason) = active_generation.start_control.cancel_reason() else {
            return Err(driver);
        };
        let TurnLifecycleState::Starting { generation, lease } =
            std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("starting state was authenticated before replacement");
        };
        let start_control = Arc::clone(&generation.start_control);
        let lifecycle_generation = generation.clone();
        self.state = TurnLifecycleState::Poisoned {
            _generation: generation,
            _turn_context: None,
            _lease: lease,
        };
        start_control.finish(TurnStartOutcome::Poisoned(reason));
        lifecycle_generation.finish_lifecycle();
        drop(driver);
        Ok(())
    }

    pub(crate) fn begin_finalization(
        &mut self,
        generation: &TurnGeneration,
        turn_context: &Arc<TurnContext>,
    ) -> Option<(R, TurnFinalization)> {
        let TurnLifecycleState::Running { owner, .. } = &self.state else {
            return None;
        };
        if !owner.matches(generation, turn_context) {
            return None;
        }
        let TurnLifecycleState::Running { owner, lease, task } =
            std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("running state was authenticated before replacement");
        };
        let token = Arc::new(AtomicBool::new(true));
        let finalization = TurnFinalization {
            token: Arc::clone(&token),
            generation: owner.generation.clone(),
        };
        self.state = TurnLifecycleState::Finalizing {
            owner,
            lease,
            token,
        };
        Some((task, finalization))
    }

    pub(crate) fn complete_finalization(
        &mut self,
        finalization: TurnFinalization,
    ) -> Result<(), TurnFinalization> {
        if !self.authenticate_finalization(&finalization)
            || finalization
                .token
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(finalization);
        }
        let TurnLifecycleState::Finalizing { owner, lease, .. } =
            std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("finalizing state was authenticated before replacement");
        };
        drop(lease);
        owner.generation.finish_lifecycle();
        Ok(())
    }

    pub(crate) fn poison_finalization(
        &mut self,
        finalization: TurnFinalization,
    ) -> Result<(), TurnFinalization> {
        if !self.authenticate_finalization(&finalization)
            || finalization
                .token
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return Err(finalization);
        }
        self.poison_current_finalization();
        Ok(())
    }

    /// Poisons an exact finalizing generation whose async driver was lost.
    pub(crate) fn poison_finalization_for_turn(
        &mut self,
        generation: &TurnGeneration,
        turn_context: &Arc<TurnContext>,
    ) -> bool {
        let TurnLifecycleState::Finalizing { owner, .. } = &self.state else {
            return false;
        };
        if !owner.matches(generation, turn_context) {
            return false;
        }
        self.poison_current_finalization();
        true
    }

    fn poison_current_finalization(&mut self) {
        let TurnLifecycleState::Finalizing {
            owner,
            lease,
            token,
        } = std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("finalizing state was authenticated before replacement");
        };
        token.store(false, Ordering::Release);
        let lifecycle_generation = owner.generation.clone();
        self.state = TurnLifecycleState::Poisoned {
            _generation: owner.generation,
            _turn_context: Some(owner.turn_context),
            _lease: lease,
        };
        lifecycle_generation.finish_lifecycle();
    }

    fn authenticate_finalization(&self, finalization: &TurnFinalization) -> bool {
        let TurnLifecycleState::Finalizing { token, .. } = &self.state else {
            return false;
        };
        Arc::ptr_eq(token, &finalization.token)
    }
}

#[cfg(test)]
#[path = "turn_lifecycle_tests.rs"]
mod tests;

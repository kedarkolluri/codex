//! Exact-identity state transitions for the single task slot owned by a session.
use crate::session::turn_context::TurnContext;
use crate::state::TurnState;
use codex_protocol::protocol::TurnAbortReason;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use tokio::sync::Mutex;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
/// Exact identity for one admission to the task slot.
#[derive(Clone)]
pub(crate) struct TurnGeneration {
    token: Arc<()>,
    turn_state: Arc<Mutex<TurnState>>,
    start_control: Arc<TurnStartControl>,
}
impl TurnGeneration {
    fn matches(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.token, &other.token)
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
            self.outcome_tx.send_replace(Some(outcome)).is_none(),
            "task start finished twice"
        );
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
    token: Arc<()>,
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
        token: Arc<()>,
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
    pub(crate) fn start(&mut self, lease: L) -> Result<TurnStartDriver, L> {
        if !matches!(self.state, TurnLifecycleState::Idle) {
            return Err(lease);
        }
        let generation = TurnGeneration {
            token: Arc::new(()),
            turn_state: Arc::new(Mutex::new(TurnState::default())),
            start_control: Arc::new(TurnStartControl::new()),
        };
        self.state = TurnLifecycleState::Starting {
            generation: generation.clone(),
            lease,
        };
        Ok(TurnStartDriver { generation })
    }

    pub(crate) fn cancel_start(&mut self, reason: TurnAbortReason) -> Option<TurnGeneration> {
        let TurnLifecycleState::Starting { generation, .. } = &mut self.state else {
            return None;
        };
        generation.start_control.request_cancel(reason);
        Some(generation.clone())
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

    /// Poisons an exact cancelled start whose linear driver was lost.
    pub(crate) fn poison_abandoned_start(&mut self, generation: &TurnGeneration) -> bool {
        let TurnLifecycleState::Starting {
            generation: active_generation,
            ..
        } = &self.state
        else {
            return false;
        };
        if !active_generation.matches(generation) {
            return false;
        }
        let Some(reason) = active_generation.start_control.cancel_reason() else {
            return false;
        };
        let TurnLifecycleState::Starting { generation, lease } =
            std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("starting state was authenticated before replacement");
        };
        let start_control = Arc::clone(&generation.start_control);
        self.state = TurnLifecycleState::Poisoned {
            _generation: generation,
            _turn_context: None,
            _lease: lease,
        };
        start_control.finish(TurnStartOutcome::Poisoned(reason));
        true
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
        let token = Arc::new(());
        let finalization = TurnFinalization {
            token: Arc::clone(&token),
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
        if !self.authenticate_finalization(&finalization) {
            return Err(finalization);
        }
        let TurnLifecycleState::Finalizing { lease, .. } =
            std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("finalizing state was authenticated before replacement");
        };
        drop(lease);
        Ok(())
    }

    pub(crate) fn poison_finalization(
        &mut self,
        finalization: TurnFinalization,
    ) -> Result<(), TurnFinalization> {
        if !self.authenticate_finalization(&finalization) {
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
        let TurnLifecycleState::Finalizing { owner, lease, .. } =
            std::mem::replace(&mut self.state, TurnLifecycleState::Idle)
        else {
            unreachable!("finalizing state was authenticated before replacement");
        };
        self.state = TurnLifecycleState::Poisoned {
            _generation: owner.generation,
            _turn_context: Some(owner.turn_context),
            _lease: lease,
        };
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

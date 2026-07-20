//! Session-owned lifetime management for detached workflow runs.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::marker::PhantomData;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;

use futures::FutureExt;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default)]
pub(super) struct WorkflowTaskManager {
    inner: Arc<WorkflowTaskManagerInner>,
}

#[derive(Default)]
struct WorkflowTaskManagerInner {
    root_cancellation: CancellationToken,
    state: Mutex<WorkflowTaskState>,
}

#[derive(Default)]
struct WorkflowTaskState {
    shutting_down: bool,
    next_generation: u64,
    tasks: HashMap<String, WorkflowTaskEntry>,
}

struct WorkflowTaskEntry {
    generation: u64,
    cancellation: WorkflowCancellation,
    requested_cause: Option<WorkflowCancellationCause>,
    done: CancellationToken,
}

const CANCELLATION_CAUSE_NONE: u8 = 0;
const CANCELLATION_CAUSE_USER_STOP: u8 = 1;
const CANCELLATION_CAUSE_INTERRUPTED: u8 = 2;
const CANCELLATION_CAUSE_PAUSE: u8 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkflowCancellationCause {
    UserStop,
    Pause,
    Interrupted,
}

/// Cancellation token paired with an immutable first-writer cause.
///
/// Authenticated run-stop writes [`WorkflowCancellationCause::UserStop`], while
/// session shutdown, foreground cancellation, and admission cleanup write
/// [`WorkflowCancellationCause::Interrupted`]. Later cancellation requests may
/// join cleanup but cannot relabel the terminal outcome.
#[derive(Clone)]
pub(super) struct WorkflowCancellation {
    token: CancellationToken,
    cause: Arc<AtomicU8>,
    controller_resolution: Arc<AtomicU8>,
}

impl WorkflowCancellation {
    fn child_of(root: &CancellationToken) -> Self {
        Self {
            token: root.child_token(),
            cause: Arc::new(AtomicU8::new(CANCELLATION_CAUSE_NONE)),
            controller_resolution: Arc::new(AtomicU8::new(CANCELLATION_CAUSE_NONE)),
        }
    }

    pub(super) fn interrupted(token: CancellationToken) -> Self {
        Self {
            token,
            cause: Arc::new(AtomicU8::new(CANCELLATION_CAUSE_INTERRUPTED)),
            controller_resolution: Arc::new(AtomicU8::new(CANCELLATION_CAUSE_NONE)),
        }
    }

    pub(super) fn pre_cancelled(cause: WorkflowCancellationCause) -> Self {
        let cancellation = Self {
            token: CancellationToken::new(),
            cause: Arc::new(AtomicU8::new(CANCELLATION_CAUSE_NONE)),
            controller_resolution: Arc::new(AtomicU8::new(CANCELLATION_CAUSE_NONE)),
        };
        cancellation.request(cause);
        cancellation
    }

    pub(super) async fn cancelled(&self) -> WorkflowCancellationCause {
        self.token.cancelled().await;
        self.cause()
    }

    fn request(&self, cause: WorkflowCancellationCause) {
        let encoded = match cause {
            WorkflowCancellationCause::UserStop => CANCELLATION_CAUSE_USER_STOP,
            WorkflowCancellationCause::Pause => CANCELLATION_CAUSE_PAUSE,
            WorkflowCancellationCause::Interrupted => CANCELLATION_CAUSE_INTERRUPTED,
        };
        let _ = self.cause.compare_exchange(
            CANCELLATION_CAUSE_NONE,
            encoded,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        self.token.cancel();
    }

    /// Record whether the authenticated stop actually won the durable terminal race.
    ///
    /// The task remains registered while cleanup persists its terminal metadata,
    /// so [`WorkflowTaskManager::cancel_run`] can wait for this resolution before
    /// reporting `Applied` to its caller.
    pub(super) fn resolve_user_stop(&self, stopped: bool) {
        self.resolve_controller(WorkflowCancellationCause::UserStop, stopped);
    }

    /// Record whether an authenticated pause won the durable terminal race.
    pub(super) fn resolve_pause(&self, paused: bool) {
        self.resolve_controller(WorkflowCancellationCause::Pause, paused);
    }

    fn resolve_controller(&self, cause: WorkflowCancellationCause, won: bool) {
        if !won {
            return;
        }
        let resolution = match cause {
            WorkflowCancellationCause::UserStop => CANCELLATION_CAUSE_USER_STOP,
            WorkflowCancellationCause::Pause => CANCELLATION_CAUSE_PAUSE,
            WorkflowCancellationCause::Interrupted => return,
        };
        let _ = self.controller_resolution.compare_exchange(
            CANCELLATION_CAUSE_NONE,
            resolution,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn controller_won(&self, cause: WorkflowCancellationCause) -> bool {
        let expected = match cause {
            WorkflowCancellationCause::UserStop => CANCELLATION_CAUSE_USER_STOP,
            WorkflowCancellationCause::Pause => CANCELLATION_CAUSE_PAUSE,
            WorkflowCancellationCause::Interrupted => return false,
        };
        self.controller_resolution.load(Ordering::Acquire) == expected
    }

    fn cause(&self) -> WorkflowCancellationCause {
        match self.cause.load(Ordering::Acquire) {
            CANCELLATION_CAUSE_USER_STOP => WorkflowCancellationCause::UserStop,
            CANCELLATION_CAUSE_PAUSE => WorkflowCancellationCause::Pause,
            CANCELLATION_CAUSE_NONE | CANCELLATION_CAUSE_INTERRUPTED => {
                WorkflowCancellationCause::Interrupted
            }
            _ => WorkflowCancellationCause::Interrupted,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkflowTaskStartError {
    ShuttingDown,
    AlreadyRunning,
}

impl fmt::Display for WorkflowTaskStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShuttingDown => formatter.write_str("workflow task manager is shutting down"),
            Self::AlreadyRunning => formatter.write_str("workflow run is already active"),
        }
    }
}

impl std::error::Error for WorkflowTaskStartError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkflowTaskCancelOutcome {
    Applied,
    AlreadyRequested,
    NotRunning,
}

pub(super) struct WorkflowTaskRejected<Start> {
    error: WorkflowTaskStartError,
    start: Start,
}

impl<Start> fmt::Debug for WorkflowTaskRejected<Start> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowTaskRejected")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl<Start> WorkflowTaskRejected<Start> {
    pub(super) fn into_parts(self) -> (WorkflowTaskStartError, Start) {
        (self.error, self.start)
    }
}

#[derive(Debug)]
pub(super) struct WorkflowTaskHandle<T, E> {
    run_id: String,
    _result: PhantomData<fn() -> Result<T, E>>,
}

impl<T, E> WorkflowTaskHandle<T, E> {
    pub(super) fn run_id(&self) -> &str {
        &self.run_id
    }
}

impl WorkflowTaskManager {
    /// Start a session-owned task while preserving its constructor on admission failure.
    ///
    /// Dropping the handle only discards that caller's result; the task continues. The
    /// task must observe the supplied cancellation and finish its resource cleanup
    /// before returning. Workflow startup uses the recovered constructor to synchronously
    /// cancel and clean a cell durably initialized just before a concurrent shutdown.
    pub(super) fn start_recoverable<T, E, Start, Task>(
        &self,
        run_id: String,
        start: Start,
    ) -> Result<WorkflowTaskHandle<T, E>, WorkflowTaskRejected<Start>>
    where
        T: Send + 'static,
        E: Send + 'static,
        Start: FnOnce(WorkflowCancellation) -> Task + Send + 'static,
        Task: Future<Output = Result<T, E>> + Send + 'static,
    {
        let (start_tx, start_rx) = oneshot::channel();
        let cancellation = WorkflowCancellation::child_of(&self.inner.root_cancellation);
        let done = CancellationToken::new();
        let generation = {
            let mut state = self.lock_state();
            if state.shutting_down {
                return Err(WorkflowTaskRejected {
                    error: WorkflowTaskStartError::ShuttingDown,
                    start,
                });
            }
            if state.tasks.contains_key(&run_id) {
                return Err(WorkflowTaskRejected {
                    error: WorkflowTaskStartError::AlreadyRunning,
                    start,
                });
            }
            let generation = state.next_generation;
            state.next_generation = state.next_generation.wrapping_add(1);
            state.tasks.insert(
                run_id.clone(),
                WorkflowTaskEntry {
                    generation,
                    cancellation: cancellation.clone(),
                    requested_cause: None,
                    done: done.clone(),
                },
            );
            generation
        };

        let inner = Arc::clone(&self.inner);
        let task_run_id = run_id.clone();
        tokio::spawn(async move {
            if start_rx.await.is_err() {
                finish_task(&inner, &task_run_id, generation, &done);
                return;
            }
            let task = catch_unwind(AssertUnwindSafe(|| start(cancellation)));
            let _output = match task {
                Ok(task) => AssertUnwindSafe(task).catch_unwind().await.ok(),
                Err(_) => None,
            };
            finish_task(&inner, &task_run_id, generation, &done);
        });
        let _ = start_tx.send(());

        Ok(WorkflowTaskHandle {
            run_id,
            _result: PhantomData,
        })
    }

    /// Cancel one run and wait for its existing cleanup path to finish.
    ///
    /// `Applied` means that cleanup confirmed the explicit stopped terminal
    /// won. If natural completion won after the task lookup but before the stop
    /// was claimed durably, this returns `NotRunning`.
    pub(super) async fn cancel_run(&self, run_id: &str) -> WorkflowTaskCancelOutcome {
        self.request_run_control(run_id, WorkflowCancellationCause::UserStop)
            .await
    }

    /// Pause one run and wait for its child-cleanup path and durable checkpoint.
    pub(super) async fn pause_run(&self, run_id: &str) -> WorkflowTaskCancelOutcome {
        self.request_run_control(run_id, WorkflowCancellationCause::Pause)
            .await
    }

    async fn request_run_control(
        &self,
        run_id: &str,
        cause: WorkflowCancellationCause,
    ) -> WorkflowTaskCancelOutcome {
        let (outcome, cancellation, done) = {
            let mut state = self.lock_state();
            let Some(task) = state.tasks.get_mut(run_id) else {
                return WorkflowTaskCancelOutcome::NotRunning;
            };
            let outcome = match task.requested_cause {
                Some(requested) if requested == cause => {
                    WorkflowTaskCancelOutcome::AlreadyRequested
                }
                Some(_) => WorkflowTaskCancelOutcome::NotRunning,
                None => {
                    task.requested_cause = Some(cause);
                    // Publish the immutable cause while the task-map decision is
                    // still locked. Otherwise a different controller can win
                    // the atomic between this first-writer decision and the
                    // token request, relabeling the terminal outcome.
                    task.cancellation.request(cause);
                    WorkflowTaskCancelOutcome::Applied
                }
            };
            (outcome, task.cancellation.clone(), task.done.clone())
        };

        done.cancelled().await;
        if !cancellation.controller_won(cause) {
            WorkflowTaskCancelOutcome::NotRunning
        } else {
            outcome
        }
    }

    /// Stop admission, cancel every run, and wait for their cleanup paths.
    pub(super) async fn shutdown(&self) {
        let done = {
            let mut state = self.lock_state();
            state.shutting_down = true;
            state
                .tasks
                .values_mut()
                .map(|task| {
                    if task.requested_cause.is_none() {
                        task.requested_cause = Some(WorkflowCancellationCause::Interrupted);
                        task.cancellation
                            .request(WorkflowCancellationCause::Interrupted);
                    }
                    task.done.clone()
                })
                .collect::<Vec<_>>()
        };
        self.inner.root_cancellation.cancel();
        for done in done {
            done.cancelled().await;
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, WorkflowTaskState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for WorkflowTaskManagerInner {
    fn drop(&mut self) {
        self.root_cancellation.cancel();
    }
}

fn finish_task(
    inner: &WorkflowTaskManagerInner,
    run_id: &str,
    generation: u64,
    done: &CancellationToken,
) {
    let mut state = inner
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if state
        .tasks
        .get(run_id)
        .is_some_and(|task| task.generation == generation)
    {
        state.tasks.remove(run_id);
    }
    drop(state);
    done.cancel();
}

#[cfg(test)]
#[path = "workflow_tasks_tests.rs"]
mod tests;

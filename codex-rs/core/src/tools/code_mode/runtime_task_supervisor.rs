//! Session-scoped ownership for code-mode observer tasks.

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

pub(super) struct RuntimeTaskSupervisor {
    admission: Mutex<()>,
    shutting_down: AtomicBool,
    cancellation: CancellationToken,
    tasks: TaskTracker,
}

#[must_use = "dropping a runtime task permit releases its shutdown join reservation"]
pub(super) struct RuntimeTaskPermit {
    cancellation: CancellationToken,
    _task: TaskTrackerToken,
}

impl RuntimeTaskSupervisor {
    pub(super) fn new() -> Self {
        Self {
            admission: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
            cancellation: CancellationToken::new(),
            tasks: TaskTracker::new(),
        }
    }

    pub(super) fn reserve(&self) -> Option<RuntimeTaskPermit> {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.is_shutting_down() {
            return None;
        }
        Some(RuntimeTaskPermit {
            cancellation: self.cancellation.child_token(),
            _task: self.tasks.token(),
        })
    }

    pub(super) fn begin_shutdown(&self) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.shutting_down.swap(true, Ordering::AcqRel) {
            return;
        }
        self.cancellation.cancel();
        self.tasks.close();
    }

    pub(super) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::Acquire)
    }

    pub(super) async fn wait(&self) {
        self.tasks.wait().await;
    }
}

impl Drop for RuntimeTaskSupervisor {
    fn drop(&mut self) {
        self.begin_shutdown();
    }
}

impl RuntimeTaskPermit {
    pub(super) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }
}

#[cfg(test)]
#[path = "runtime_task_supervisor_tests.rs"]
mod tests;

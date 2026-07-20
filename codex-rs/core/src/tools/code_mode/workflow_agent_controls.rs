//! Exact, attempt-scoped control for live Dynamic Workflow agents.
//!
//! The registry belongs to one [`CodeModeService`](super::CodeModeService), which in turn belongs
//! to one root thread session. That ownership is the authorization boundary: callers can control
//! only attempts registered by that exact session, and lookups never infer a latest run, node, or
//! generation.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

pub(crate) use codex_code_mode::WORKFLOW_AGENT_MAX_RETRIES as MAX_WORKFLOW_AGENT_RETRIES;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

/// User operation applied to one exact live workflow-agent attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowAgentControlAction {
    Skip,
    Retry,
}

/// Result returned after the selected attempt's child, worktree, and scheduler permit are clean.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowAgentControlDisposition {
    /// The logical `agent()` call will settle to JavaScript `null`.
    Skipped,
    /// The logical promise remains unsettled while this fresh attempt is admitted normally.
    RetryScheduled { attempt: u32 },
    /// The sixth attempt was cleaned and the logical call failed without spawning a seventh.
    RetryLimitReached,
    /// The exact attempt is absent, stale, complete, foreign, malformed, or lost another race.
    Unavailable,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AttemptKey {
    run_id: String,
    node_id: u64,
    attempt: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptClaim {
    User(WorkflowAgentControlAction),
    RunCancellation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AttemptFinish {
    Natural,
    UserSkip,
    UserRetry { next_attempt: u32 },
    RetryLimitReached,
    RunCancellation,
    PersistenceFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptStage {
    Pending,
    Live,
    Claimed(AttemptClaim),
    Finalizing(AttemptFinish),
    Cleaned(AttemptFinish),
}

struct AttemptEntry {
    cancellation: CancellationToken,
    stage: Mutex<AttemptStage>,
    cleaned: Notify,
}

impl AttemptEntry {
    fn new(cancellation: CancellationToken) -> Self {
        Self {
            cancellation,
            stage: Mutex::new(AttemptStage::Pending),
            cleaned: Notify::new(),
        }
    }

    fn activate(&self) {
        let mut stage = self
            .stage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *stage == AttemptStage::Pending {
            *stage = AttemptStage::Live;
        }
    }

    fn claim_user(&self, action: WorkflowAgentControlAction) -> UserClaim {
        let mut stage = self
            .stage
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *stage {
            AttemptStage::Pending => UserClaim::Unavailable,
            AttemptStage::Live => {
                *stage = AttemptStage::Claimed(AttemptClaim::User(action));
                UserClaim::First
            }
            AttemptStage::Claimed(AttemptClaim::User(existing)) if existing == action => {
                UserClaim::Join
            }
            AttemptStage::Cleaned(finish) if finish_matches_action(finish, action) => {
                UserClaim::Join
            }
            AttemptStage::Claimed(_) | AttemptStage::Finalizing(_) | AttemptStage::Cleaned(_) => {
                UserClaim::Conflict
            }
        }
    }

    fn claim_run_cancellation(&self) {
        let claimed = {
            let mut stage = self
                .stage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(*stage, AttemptStage::Pending | AttemptStage::Live) {
                *stage = AttemptStage::Claimed(AttemptClaim::RunCancellation);
                true
            } else {
                false
            }
        };
        if claimed {
            self.cancellation.cancel();
        }
    }

    fn decide_after_cleanup(&self, attempt: u32, run_cancelled: bool) -> AttemptFinish {
        {
            let mut stage = self
                .stage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let finish =
                match *stage {
                    AttemptStage::Pending if run_cancelled => AttemptFinish::RunCancellation,
                    AttemptStage::Pending => AttemptFinish::Natural,
                    AttemptStage::Live if run_cancelled => AttemptFinish::RunCancellation,
                    AttemptStage::Live => AttemptFinish::Natural,
                    AttemptStage::Claimed(AttemptClaim::RunCancellation) => {
                        AttemptFinish::RunCancellation
                    }
                    AttemptStage::Claimed(AttemptClaim::User(WorkflowAgentControlAction::Skip)) => {
                        AttemptFinish::UserSkip
                    }
                    AttemptStage::Claimed(AttemptClaim::User(
                        WorkflowAgentControlAction::Retry,
                    )) if run_cancelled => AttemptFinish::RunCancellation,
                    AttemptStage::Claimed(AttemptClaim::User(
                        WorkflowAgentControlAction::Retry,
                    )) if attempt >= MAX_WORKFLOW_AGENT_RETRIES => AttemptFinish::RetryLimitReached,
                    AttemptStage::Claimed(AttemptClaim::User(
                        WorkflowAgentControlAction::Retry,
                    )) => AttemptFinish::UserRetry {
                        next_attempt: attempt.saturating_add(1),
                    },
                    AttemptStage::Finalizing(finish) | AttemptStage::Cleaned(finish) => {
                        return finish;
                    }
                };
            *stage = AttemptStage::Finalizing(finish);
            finish
        }
    }

    fn acknowledge(&self, finish: AttemptFinish) {
        {
            let mut stage = self
                .stage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if *stage == AttemptStage::Finalizing(finish) {
                *stage = AttemptStage::Cleaned(finish);
            } else if *stage != AttemptStage::Cleaned(finish) {
                return;
            }
        }
        self.cleaned.notify_waiters();
    }

    fn acknowledge_persistence_failure(&self) {
        {
            let mut stage = self
                .stage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match *stage {
                AttemptStage::Finalizing(_) => {
                    *stage = AttemptStage::Cleaned(AttemptFinish::PersistenceFailure);
                }
                AttemptStage::Cleaned(AttemptFinish::PersistenceFailure) => {}
                AttemptStage::Pending
                | AttemptStage::Live
                | AttemptStage::Claimed(_)
                | AttemptStage::Cleaned(_) => return,
            }
        }
        self.cleaned.notify_waiters();
    }

    async fn wait_until_cleaned(&self) -> AttemptFinish {
        loop {
            // Register before checking the state so a cleanup notification cannot be lost between
            // the check and the await.
            let notified = self.cleaned.notified();
            let stage = *self
                .stage
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let AttemptStage::Cleaned(finish) = stage {
                return finish;
            }
            notified.await;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UserClaim {
    First,
    Join,
    Conflict,
    Unavailable,
}

fn finish_matches_action(finish: AttemptFinish, action: WorkflowAgentControlAction) -> bool {
    matches!(
        (finish, action),
        (AttemptFinish::UserSkip, WorkflowAgentControlAction::Skip)
            | (
                AttemptFinish::UserRetry { .. } | AttemptFinish::RetryLimitReached,
                WorkflowAgentControlAction::Retry
            )
    )
}

/// Thread-scoped set of exact live attempts.
#[derive(Default)]
pub(super) struct WorkflowAgentControlRegistry {
    attempts: Mutex<HashMap<AttemptKey, Arc<AttemptEntry>>>,
}

impl WorkflowAgentControlRegistry {
    pub(super) fn register_attempt(
        self: &Arc<Self>,
        run_id: &str,
        node_id: u64,
        attempt: u32,
        cancellation: CancellationToken,
    ) -> Option<WorkflowAgentAttemptRegistration> {
        let run_id = canonical_run_id(run_id)?;
        if attempt > MAX_WORKFLOW_AGENT_RETRIES {
            return None;
        }
        let key = AttemptKey {
            run_id,
            node_id,
            attempt,
        };
        let entry = Arc::new(AttemptEntry::new(cancellation));
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attempts.contains_key(&key) {
            return None;
        }
        attempts.insert(key.clone(), Arc::clone(&entry));
        Some(WorkflowAgentAttemptRegistration {
            registry: Arc::clone(self),
            key,
            entry,
        })
    }

    pub(super) async fn control(
        &self,
        run_id: &str,
        node_id: u64,
        attempt: u32,
        action: WorkflowAgentControlAction,
    ) -> WorkflowAgentControlDisposition {
        let Some(run_id) = canonical_run_id(run_id) else {
            return WorkflowAgentControlDisposition::Unavailable;
        };
        let key = AttemptKey {
            run_id,
            node_id,
            attempt,
        };
        let entry = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            .cloned();
        let Some(entry) = entry else {
            return WorkflowAgentControlDisposition::Unavailable;
        };
        match entry.claim_user(action) {
            UserClaim::First => entry.cancellation.cancel(),
            UserClaim::Join => {}
            // A losing action still joins the winner's cleanup barrier. This prevents a caller
            // from observing "unavailable" before the selected attempt actually releases its
            // child/worktree/permit, while the response remains one bounded non-applied value.
            UserClaim::Conflict => {}
            UserClaim::Unavailable => return WorkflowAgentControlDisposition::Unavailable,
        }
        match (entry.wait_until_cleaned().await, action) {
            (AttemptFinish::UserSkip, WorkflowAgentControlAction::Skip) => {
                WorkflowAgentControlDisposition::Skipped
            }
            (AttemptFinish::UserRetry { next_attempt }, WorkflowAgentControlAction::Retry) => {
                WorkflowAgentControlDisposition::RetryScheduled {
                    attempt: next_attempt,
                }
            }
            (AttemptFinish::RetryLimitReached, WorkflowAgentControlAction::Retry) => {
                WorkflowAgentControlDisposition::RetryLimitReached
            }
            (
                AttemptFinish::Natural
                | AttemptFinish::RunCancellation
                | AttemptFinish::UserSkip
                | AttemptFinish::UserRetry { .. }
                | AttemptFinish::RetryLimitReached
                | AttemptFinish::PersistenceFailure,
                _,
            ) => WorkflowAgentControlDisposition::Unavailable,
        }
    }

    fn remove(&self, key: &AttemptKey, entry: &Arc<AttemptEntry>) {
        let mut attempts = self
            .attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if attempts
            .get(key)
            .is_some_and(|registered| Arc::ptr_eq(registered, entry))
        {
            attempts.remove(key);
        }
    }
}

pub(super) struct WorkflowAgentAttemptRegistration {
    registry: Arc<WorkflowAgentControlRegistry>,
    key: AttemptKey,
    entry: Arc<AttemptEntry>,
}

impl WorkflowAgentAttemptRegistration {
    pub(super) fn activation(&self) -> WorkflowAgentAttemptActivation {
        WorkflowAgentAttemptActivation {
            entry: Arc::clone(&self.entry),
        }
    }

    #[cfg(test)]
    fn activate(&self) {
        self.entry.activate();
    }

    pub(super) fn claim_run_cancellation(&self) {
        self.entry.claim_run_cancellation();
    }

    pub(super) fn decide_after_cleanup(&self, run_cancelled: bool) -> AttemptFinish {
        self.entry
            .decide_after_cleanup(self.key.attempt, run_cancelled)
    }

    pub(super) fn acknowledge(self, finish: AttemptFinish) {
        self.entry.acknowledge(finish);
        self.registry.remove(&self.key, &self.entry);
    }

    pub(super) fn acknowledge_persistence_failure(self) {
        self.entry.acknowledge_persistence_failure();
        self.registry.remove(&self.key, &self.entry);
    }

    #[cfg(test)]
    fn finish_after_cleanup(self, run_cancelled: bool) -> AttemptFinish {
        let finish = self.decide_after_cleanup(run_cancelled);
        self.acknowledge(finish);
        finish
    }
}

#[derive(Clone)]
pub(super) struct WorkflowAgentAttemptActivation {
    entry: Arc<AttemptEntry>,
}

impl WorkflowAgentAttemptActivation {
    pub(super) fn activate(&self) {
        self.entry.activate();
    }
}

impl Drop for WorkflowAgentAttemptRegistration {
    fn drop(&mut self) {
        // A dropped execution path is no longer controllable. The attempt token is cancelled so a
        // child cannot outlive the registry entry, then waiters observe a joined run cancellation.
        self.entry.claim_run_cancellation();
        let finish = self
            .entry
            .decide_after_cleanup(self.key.attempt, /*run_cancelled*/ true);
        self.entry.acknowledge(finish);
        self.registry.remove(&self.key, &self.entry);
    }
}

fn canonical_run_id(run_id: &str) -> Option<String> {
    let parsed = uuid::Uuid::parse_str(run_id).ok()?;
    let canonical = parsed.hyphenated().to_string();
    (canonical == run_id).then_some(canonical)
}

#[cfg(test)]
#[path = "workflow_agent_controls_tests.rs"]
mod tests;

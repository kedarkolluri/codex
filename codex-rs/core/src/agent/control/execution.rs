use super::AgentControl;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use tokio::sync::watch;

pub(super) struct AgentExecutionLimiter {
    active: AtomicUsize,
    max_threads: OnceLock<usize>,
    release_tx: watch::Sender<u64>,
}

pub(crate) struct AgentExecutionGuard {
    limiter: Arc<AgentExecutionLimiter>,
}

/// Atomic execution-capacity admission result for a prospective turn.
pub(crate) enum AgentExecutionAdmission {
    Unrestricted,
    Admitted(AgentExecutionGuard),
    AtCapacity(AgentExecutionCapacityWaiter),
}

/// Pre-subscribed notification for retrying a failed capacity admission.
pub(crate) struct AgentExecutionCapacityWaiter {
    max_threads: usize,
    release_rx: watch::Receiver<u64>,
}

impl AgentExecutionCapacityWaiter {
    pub(crate) fn into_limit_error(self) -> CodexErr {
        CodexErr::AgentLimitReached {
            max_threads: self.max_threads,
        }
    }

    pub(crate) async fn wait_for_release(mut self) {
        let _ = self.release_rx.changed().await;
    }
}

impl Drop for AgentExecutionGuard {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(/*val*/ 1, Ordering::AcqRel);
        self.limiter
            .release_tx
            .send_modify(|release| *release = release.wrapping_add(/*rhs*/ 1));
    }
}

impl AgentControl {
    pub(crate) async fn ensure_execution_capacity_for_op(
        &self,
        thread_id: ThreadId,
        op: &Op,
    ) -> CodexResult<()> {
        self.ensure_execution_capacity_for_turn_start(thread_id, op_starts_turn(op))
            .await
    }

    pub(super) async fn ensure_execution_capacity_for_turn_start(
        &self,
        thread_id: ThreadId,
        starts_turn: bool,
    ) -> CodexResult<()> {
        if !starts_turn {
            return Ok(());
        }
        let state = self.upgrade()?;
        let thread = state.get_thread(thread_id).await?;
        if thread.session.active_turn.lock().await.has_active_turn() {
            return Ok(());
        }
        let config = thread.session.get_config().await;
        let multi_agent_version = thread
            .multi_agent_version()
            .unwrap_or_else(|| config.multi_agent_version_from_features());
        self.ensure_execution_capacity(multi_agent_version, &thread.session_source)
    }

    pub(crate) fn ensure_execution_capacity(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<()> {
        if !is_execution_limited(multi_agent_version, session_source) {
            return Ok(());
        }
        let max_threads = self.agent_execution_limiter.max_threads();
        if self.agent_execution_limiter.has_capacity() {
            Ok(())
        } else {
            Err(CodexErr::AgentLimitReached { max_threads })
        }
    }

    #[cfg(test)]
    pub(crate) fn execution_guard(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> Option<AgentExecutionGuard> {
        is_execution_limited(multi_agent_version, session_source)
            .then(|| Arc::clone(&self.agent_execution_limiter).guard())
    }

    /// Atomically admits a limited turn or returns a pre-subscribed release waiter.
    pub(crate) fn execution_admission(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> AgentExecutionAdmission {
        if !is_execution_limited(multi_agent_version, session_source) {
            return AgentExecutionAdmission::Unrestricted;
        }
        let max_threads = self.agent_execution_limiter.max_threads();
        // Subscribe before the atomic admission attempt. If the capacity owner
        // drops after the failed CAS but before the caller polls the waiter,
        // the receiver still observes that release.
        let release_rx = self.agent_execution_limiter.release_tx.subscribe();
        match Arc::clone(&self.agent_execution_limiter).try_guard() {
            Some(guard) => AgentExecutionAdmission::Admitted(guard),
            None => AgentExecutionAdmission::AtCapacity(AgentExecutionCapacityWaiter {
                max_threads,
                release_rx,
            }),
        }
    }

    // Used by the next stacked task-start reservation change.
    #[allow(dead_code)]
    pub(crate) fn try_execution_guard(
        &self,
        multi_agent_version: MultiAgentVersion,
        session_source: &SessionSource,
    ) -> CodexResult<Option<AgentExecutionGuard>> {
        if !is_execution_limited(multi_agent_version, session_source) {
            return Ok(None);
        }
        let max_threads = self.agent_execution_limiter.max_threads();
        Arc::clone(&self.agent_execution_limiter)
            .try_guard()
            .map(Some)
            .ok_or(CodexErr::AgentLimitReached { max_threads })
    }
}

impl Default for AgentExecutionLimiter {
    fn default() -> Self {
        let (release_tx, _) = watch::channel(/*init*/ 0);
        Self {
            active: AtomicUsize::new(/*v*/ 0),
            max_threads: OnceLock::new(),
            release_tx,
        }
    }
}

impl AgentExecutionLimiter {
    pub(super) fn initialize(&self, max_threads: usize) {
        self.max_threads.get_or_init(|| max_threads);
    }

    fn max_threads(&self) -> usize {
        self.max_threads.get().copied().unwrap_or(usize::MAX)
    }

    fn has_capacity(&self) -> bool {
        self.active.load(Ordering::Acquire) < self.max_threads()
    }

    #[cfg(test)]
    fn guard(self: Arc<Self>) -> AgentExecutionGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        AgentExecutionGuard { limiter: self }
    }

    fn try_guard(self: Arc<Self>) -> Option<AgentExecutionGuard> {
        let max_threads = self.max_threads();
        self.active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                if active < max_threads {
                    Some(active + 1)
                } else {
                    None
                }
            })
            .ok()
            .map(|_| AgentExecutionGuard { limiter: self })
    }
}

fn op_starts_turn(op: &Op) -> bool {
    matches!(op, Op::UserInput { .. })
        || matches!(op, Op::InterAgentCommunication { communication } if communication.trigger_turn)
}

fn is_execution_limited(
    multi_agent_version: MultiAgentVersion,
    session_source: &SessionSource,
) -> bool {
    multi_agent_version == MultiAgentVersion::V2
        && matches!(session_source, SessionSource::SubAgent(_))
}

#[cfg(test)]
#[path = "execution_tests.rs"]
mod tests;

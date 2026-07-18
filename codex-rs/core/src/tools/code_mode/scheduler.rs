//! Workflow concurrency scheduler (P1-scheduler-semaphore, spec §5).
//!
//! A workflow run fans out `agent()` calls (`parallel()`, `pipeline()`) that would
//! otherwise spawn an unbounded number of concurrent subagents. This module owns the
//! host-side [`tokio::sync::Semaphore`] that bounds how many `agent()` invocations may
//! be *in flight* at once, plus the admission combinator every `agent()` dispatch runs
//! through.
//!
//! ## Cap policy (spec §5 "Concurrency cap")
//!
//! `cap = min(16, available_parallelism().saturating_sub(2))`, further clamped to
//! `effective_agent_max_threads` (`config/mod.rs:1428`) via the same clamp shape as
//! [`normalize_concurrency`](crate::tools::handlers::agent_jobs) (`agent_jobs.rs:130`).
//!
//! The session-default `effective_agent_max_threads` is 6 (V1) /
//! `max_concurrent_threads_per_session - 1` (V2) — *below* the `min(16, cores-2)`
//! policy ceiling a workflow run is meant to reach. Per the §5 **cap-override note**,
//! the workflow-owned subagent tree therefore raises that clamp to
//! [`WORKFLOW_AGENT_MAX_THREADS_CEILING`] so the clamp never pulls the observed cap
//! below the intended `min(16, cores-2)` policy. The clamp is still *applied* — it is
//! simply raised so the policy cap wins for workflow runs (see
//! [`normalize_workflow_concurrency`]).
//!
//! ## Admission (spec §5 "Admission order", steps 4 & 6)
//!
//! Each admitted `agent()` **acquires a permit before spawning the child** and **drops
//! it on finalize** — on the success path *and* the failure/abort path alike. Excess
//! `agent()` calls simply `await` a permit: that wait *is* "excess queued". This mirrors
//! the working admit/reap loop in `agent_jobs.rs::run_agent_job_loop` (`:160-315`) but
//! uses a semaphore instead of manual `HashMap` slot arithmetic, because the workflow
//! host is in-process and structured rather than DB-persisted and crash-recoverable.
//!
//! [`AgentRegistry::reserve_spawn_slot`](crate::agent::registry) (`registry.rs:82`)
//! stays the **hard backstop**: it can still reject a spawn with
//! [`CodexErr::AgentLimitReached`](codex_protocol::error::CodexErr::AgentLimitReached)
//! when the shared session registry is saturated by non-workflow agents. On that error
//! the scheduler **requeues** the call — it drops the permit and retries — rather than
//! surfacing an error, so a transient registry-slot shortage never fails an `agent()`
//! call (see [`SpawnAttempt`] and [`WorkflowScheduler::admit`]).

// Wired into the production spawn path by the code-mode `CoreTurnHost::spawn_agent`
// (`delegate.rs`): every workflow `agent()` call is admitted through `admit()` into the
// `spawn_and_await_final_message` keystone. A few surfaces remain test-only for now — the
// registry-backstop `SpawnAttempt::AgentLimitReached` requeue (the keystone currently maps a
// saturated-registry spawn error to `None` rather than surfacing it) and the
// instrumentation accessors — so the module keeps a blanket `dead_code` allow rather than
// sprinkling per-item ones.
#![allow(dead_code)]

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use tokio::sync::Semaphore;

/// Upper bound on the *policy* concurrency cap, independent of the machine's core
/// count (`min(16, ...)` in the spec §5 formula).
const CONCURRENCY_POLICY_CEILING: usize = 16;

/// The clamp that workflow-owned subagent trees raise `effective_agent_max_threads` to
/// (spec §5 cap-override note). Because it is `>= CONCURRENCY_POLICY_CEILING`, the
/// `min(16, cores-2)` policy cap always wins over the session-default clamp for a
/// workflow run, while the clamp itself remains genuinely applied.
const WORKFLOW_AGENT_MAX_THREADS_CEILING: usize = CONCURRENCY_POLICY_CEILING;

/// Per-run lifetime ceiling on total `agent()` admissions (spec §5 "Lifetime cap —
/// ~1000 agents/run"). Counted monotonically; it never decrements, so completed/released
/// agents do not free budget — distinct from `AgentRegistry.total_count`, which decrements
/// on release (`registry.rs:99`).
const LIFETIME_SPAWN_CAP: usize = 1000;

/// Compute the `min(16, cores-2)` **policy** cap from a concrete core count, clamped to
/// at least 1 (a single-core box still admits one agent at a time).
fn policy_cap_from_cores(cores: usize) -> usize {
    // `clamp(1, 16)`: floor at 1 (a single-core box still admits one) and cap at 16.
    cores.saturating_sub(2).clamp(1, CONCURRENCY_POLICY_CEILING)
}

/// Clamp the policy cap to the workflow-raised `effective_agent_max_threads`, mirroring
/// the `requested.min(max_threads.max(1))` shape of
/// `agent_jobs.rs::normalize_concurrency` (`:130`). Kept as a standalone testable unit
/// so the "clamp is applied" behavior can be exercised directly.
fn normalize_workflow_concurrency(policy_cap: usize, clamp: usize) -> usize {
    policy_cap.min(clamp.max(1)).max(1)
}

/// Raise the session-default `effective_agent_max_threads` to the workflow ceiling
/// (spec §5 cap-override note). `None` (no configured limit) also raises to the ceiling.
fn raise_workflow_clamp(effective_agent_max_threads: Option<usize>) -> usize {
    effective_agent_max_threads
        .unwrap_or(0)
        .max(WORKFLOW_AGENT_MAX_THREADS_CEILING)
}

/// Compute the workflow concurrency cap from a concrete core count and the session's
/// `effective_agent_max_threads`. Split out from [`workflow_concurrency_cap`] so tests
/// pin the result without depending on the host's real `available_parallelism()`.
fn workflow_concurrency_cap_from(
    cores: usize,
    effective_agent_max_threads: Option<usize>,
) -> usize {
    let policy_cap = policy_cap_from_cores(cores);
    let clamp = raise_workflow_clamp(effective_agent_max_threads);
    normalize_workflow_concurrency(policy_cap, clamp)
}

/// The workflow concurrency cap for this host: `min(16, cores-2)` clamped by the
/// workflow-raised `effective_agent_max_threads` (spec §5). `available_parallelism()`
/// only fails when the count is unknowable, in which case we fall back to a single
/// core (cap 1).
pub(crate) fn workflow_concurrency_cap(effective_agent_max_threads: Option<usize>) -> usize {
    let cores = std::thread::available_parallelism()
        .map(std::num::NonZero::get)
        .unwrap_or(1);
    workflow_concurrency_cap_from(cores, effective_agent_max_threads)
}

/// The outcome of one spawn attempt inside [`WorkflowScheduler::admit`].
///
/// The spawn closure runs the *entire* `agent()` lifecycle under the held permit —
/// spawn the child via the registering path, then block on its event stream to
/// `TurnComplete`/`TurnAborted` (the `spawn_and_await_final_message` keystone). It
/// reports back which of two things happened:
pub(crate) enum SpawnAttempt<T> {
    /// The child was admitted, spawned, and run to finalize — on the success path
    /// (`TurnComplete`) or the failure/abort path (`TurnAborted`, a spawn/submit error
    /// mapped to `None`) alike. Either way the permit is dropped and `T` is returned to
    /// the caller. This is the terminal, non-requeued outcome.
    Finalized(T),
    /// The registry hard backstop
    /// ([`reserve_spawn_slot`](crate::agent::registry) `registry.rs:82`) rejected the
    /// spawn with [`CodexErr::AgentLimitReached`](codex_protocol::error::CodexErr).
    /// The child never started, so the scheduler drops the permit and **requeues** the
    /// call rather than failing it.
    AgentLimitReached,
}

/// The per-run lifetime cap (spec §5) was hit: `admit` rejected an `agent()` invocation
/// because [`LIFETIME_SPAWN_CAP`] admissions have already been counted in this run.
///
/// Surfaced as an `Err` from [`WorkflowScheduler::admit`] rather than a `SpawnAttempt`
/// requeue: unlike the registry backstop ([`SpawnAttempt::AgentLimitReached`]), which is a
/// transient shortage the scheduler retries, the lifetime cap is terminal and monotonic —
/// no sibling finalizing can ever free budget. The caller maps it to the `AgentCapReached`
/// throw the workflow body observes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AgentCapReached {
    /// The lifetime ceiling that was reached (always [`LIFETIME_SPAWN_CAP`] for a real run).
    pub(crate) cap: usize,
}

/// Host-side concurrency scheduler for a workflow run's `agent()` fan-out (spec §5).
///
/// Owns a fixed-capacity [`Semaphore`]; every `agent()` invocation is admitted through
/// [`admit`](Self::admit), which acquires a permit before spawning and releases it on
/// finalize. Clone-cheap: the semaphore is shared behind an [`Arc`], so the same
/// scheduler bounds *total* concurrent agents across a whole `parallel()`/`pipeline()`
/// fan-out (the single global semaphore is what makes `pipeline`'s no-barrier
/// staggering fall out for free, spec §5).
///
/// Also owns the per-run [`lifetime_spawned`](Self::lifetime_spawned) counter (spec §5
/// "Lifetime cap"): a monotonic [`AtomicUsize`] CAS-incremented at admission **before** the
/// concurrency permit, ceiling [`LIFETIME_SPAWN_CAP`], that never decrements. It is
/// `Arc`-shared across clones so a whole `parallel()`/`pipeline()` fan-out counts against a
/// single per-run budget; a fresh scheduler starts fresh at 0 (per-run, not per-session).
#[derive(Clone)]
pub(crate) struct WorkflowScheduler {
    semaphore: Arc<Semaphore>,
    cap: usize,
    /// Monotonic count of lifetime `agent()` admissions in this run (never decrements).
    lifetime_spawned: Arc<AtomicUsize>,
    /// The lifetime ceiling — [`LIFETIME_SPAWN_CAP`] for a real run; overridable in tests.
    lifetime_cap: usize,
}

impl WorkflowScheduler {
    /// Build a scheduler whose cap is `min(16, cores-2)` clamped by the workflow-raised
    /// `effective_agent_max_threads` (spec §5). Pass the parent turn's
    /// `config.effective_agent_max_threads(version)`.
    pub(crate) fn new(effective_agent_max_threads: Option<usize>) -> Self {
        Self::with_cap(workflow_concurrency_cap(effective_agent_max_threads))
    }

    /// Build a scheduler with an explicit concurrency cap and the real lifetime ceiling.
    /// Used by tests to pin a deterministic cap independent of the host's core count.
    fn with_cap(cap: usize) -> Self {
        Self::with_caps(cap, LIFETIME_SPAWN_CAP)
    }

    /// Build a scheduler with explicit concurrency and lifetime caps. Used by tests to pin
    /// a small lifetime ceiling so the "cap reached" boundary can be exercised cheaply
    /// (production always uses [`LIFETIME_SPAWN_CAP`]).
    fn with_caps(cap: usize, lifetime_cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            semaphore: Arc::new(Semaphore::new(cap)),
            cap,
            lifetime_spawned: Arc::new(AtomicUsize::new(0)),
            lifetime_cap: lifetime_cap.max(1),
        }
    }

    /// The concurrency cap — the maximum number of `agent()` calls that may be in flight
    /// at once.
    pub(crate) fn cap(&self) -> usize {
        self.cap
    }

    /// Permits not currently held. `cap()` when idle; `0` when saturated. Exposed for
    /// instrumentation/tests.
    pub(crate) fn available_permits(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// The number of lifetime `agent()` admissions counted so far in this run. Monotonic —
    /// it only ever grows, and a completed/released agent never gives budget back. Exposed
    /// for instrumentation/tests.
    pub(crate) fn lifetime_spawned(&self) -> usize {
        self.lifetime_spawned.load(Ordering::Acquire)
    }

    /// The per-run lifetime ceiling (spec §5). Once [`lifetime_spawned`](Self::lifetime_spawned)
    /// reaches it, every further admission throws [`AgentCapReached`].
    pub(crate) fn lifetime_cap(&self) -> usize {
        self.lifetime_cap
    }

    /// CAS-increment the monotonic lifetime counter, copying the `total_count` bump in
    /// `registry.rs:289 try_increment_spawned`. Returns `true` if a slot was claimed, or
    /// `false` once the run has already admitted [`lifetime_cap`](Self::lifetime_cap)
    /// agents. Never decrements.
    fn try_increment_lifetime(&self) -> bool {
        let mut current = self.lifetime_spawned.load(Ordering::Acquire);
        loop {
            if current >= self.lifetime_cap {
                return false;
            }
            match self.lifetime_spawned.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(updated) => current = updated,
            }
        }
    }

    /// Admit one `agent()` invocation.
    ///
    /// 1. **Lifetime CAS (spec §5 admission-order step 2), before the permit (step 4).**
    ///    Monotonically claim a per-run lifetime slot via
    ///    [`try_increment_lifetime`](Self::try_increment_lifetime). If the run has already
    ///    admitted [`lifetime_cap`](Self::lifetime_cap) agents, return
    ///    `Err(`[`AgentCapReached`]`)` *immediately* — without ever awaiting a concurrency
    ///    permit — so the cap throws even while the semaphore is saturated. The counter
    ///    never decrements, so this budget is spent for the life of the run.
    /// 2. Acquire a permit — awaiting when all `cap` permits are held. That wait *is*
    ///    "excess queued" (spec §5): the FIFO-fair semaphore releases queued waiters as
    ///    in-flight agents finalize.
    /// 3. Run `spawn` under the held permit. `spawn` performs the whole `agent()`
    ///    lifecycle (spawn the child + block to `TurnComplete`/`TurnAborted`) and reports
    ///    a [`SpawnAttempt`].
    /// 4. On [`SpawnAttempt::Finalized`] drop the permit and return `Ok(value)` — the
    ///    permit is released on the success path *and* the failure/abort path, because
    ///    both arrive as `Finalized`.
    /// 5. On [`SpawnAttempt::AgentLimitReached`] (the registry backstop rejected the
    ///    spawn) drop the permit and **requeue**: yield, then re-acquire and retry. The
    ///    permit is dropped *before* retrying so a sibling agent can finalize and free a
    ///    registry slot — retrying while holding the permit could otherwise self-deadlock
    ///    a saturated run. The lifetime slot is claimed **once**, before the loop, so a
    ///    requeue (the child never started) never double-counts against the lifetime cap.
    ///
    /// `spawn` is `FnMut` because it may be invoked more than once (once per requeue).
    pub(crate) async fn admit<F, Fut, T>(&self, mut spawn: F) -> Result<T, AgentCapReached>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = SpawnAttempt<T>>,
    {
        // Admission-order step 2: claim the lifetime slot BEFORE awaiting a permit (step 4).
        if !self.try_increment_lifetime() {
            return Err(AgentCapReached {
                cap: self.lifetime_cap,
            });
        }
        loop {
            // Acquire a permit. The scheduler never closes the semaphore, so `acquire`
            // resolves `Ok` in practice; `.ok()` keeps an unreachable close from
            // deadlocking the run (it degrades to running the spawn unbounded rather
            // than panicking). The permit guard releases the slot when it drops at the
            // end of the iteration.
            let permit = self.semaphore.acquire().await.ok();
            match spawn().await {
                SpawnAttempt::Finalized(value) => {
                    // Release on finalize — success AND failure/abort both land here.
                    drop(permit);
                    return Ok(value);
                }
                SpawnAttempt::AgentLimitReached => {
                    // Requeue: drop the permit first so a sibling can make progress and
                    // free a registry slot, then yield and re-acquire from the back of
                    // the fair wait queue.
                    drop(permit);
                    tokio::task::yield_now().await;
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "scheduler_tests.rs"]
mod tests;
